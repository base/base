use base_comp::{BrotliCompressor, BrotliLevel, ChannelOut};
use base_consensus_genesis::RollupConfig;
use base_protocol::{Batch, ChannelId, Frame, SingleBatch};
use tracing::debug;

use crate::{ChannelDriverConfig, ChannelDriverError};

/// Minimal batch encoding pipeline.
///
/// `ChannelDriver` accumulates [`SingleBatch`]es, then on [`flush`] compresses
/// them all into a single channel using [`ChannelOut`] with
/// [`BrotliCompressor`] at [`BrotliLevel::Brotli10`] (the compression level
/// Base uses in production) and returns the resulting [`Frame`]s.
///
/// # Single-frame limitation
///
/// The current implementation outputs all data in one frame by using
/// [`MAX_FRAME_LEN`](base_protocol::MAX_FRAME_LEN) as the frame size ceiling.
/// This is correct for action tests where batches are small. Multi-frame
/// output requires a consuming `read` implementation on the underlying
/// compressor, which is tracked separately.
///
/// # Usage
///
/// ```rust,ignore
/// let mut driver = ChannelDriver::new(rollup_config, ChannelDriverConfig::default());
/// driver.add_batch(batch_a);
/// driver.add_batch(batch_b);
/// let frames = driver.flush()?;  // → Vec<Frame> ready for L1 submission
/// ```
///
/// [`flush`]: ChannelDriver::flush
#[derive(Debug)]
pub struct ChannelDriver {
    rollup_config: RollupConfig,
    config: ChannelDriverConfig,
    /// Accumulated batches waiting to be encoded.
    pending: Vec<SingleBatch>,
}

impl ChannelDriver {
    /// Create a new [`ChannelDriver`] with the given rollup config and
    /// encoding configuration.
    pub const fn new(rollup_config: RollupConfig, config: ChannelDriverConfig) -> Self {
        Self { rollup_config, config, pending: Vec::new() }
    }

    /// Queue a [`SingleBatch`] for encoding on the next [`flush`].
    ///
    /// [`flush`]: ChannelDriver::flush
    pub fn add_batch(&mut self, batch: SingleBatch) {
        self.pending.push(batch);
    }

    /// Return the number of batches queued but not yet flushed.
    pub const fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Return `true` if no batches are queued.
    pub const fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Encode all queued batches into compressed channel frames.
    ///
    /// A fresh [`ChannelOut`] is created for each flush so channels do not
    /// span across calls. All pending batches are drained, compressed together
    /// into a single channel, and returned as a `Vec<Frame>`. After a
    /// successful flush the driver is empty and ready for the next round.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelDriverError::Empty`] if there are no queued batches.
    /// Returns [`ChannelDriverError::Channel`] if batch encoding or
    /// compression fails.
    pub fn flush(&mut self) -> Result<Vec<Frame>, ChannelDriverError> {
        if self.pending.is_empty() {
            return Err(ChannelDriverError::Empty);
        }

        let compressor = BrotliCompressor::new(BrotliLevel::Brotli10);
        let mut channel_out =
            ChannelOut::new(ChannelId::default(), &self.rollup_config, compressor);

        for batch in &self.pending {
            let timestamp = batch.timestamp;
            channel_out.add_batch(Batch::Single(batch.clone()))?;
            debug!(timestamp, "encoded batch into channel");
        }

        channel_out.flush()?;
        channel_out.close();

        let frame = channel_out.output_frame(self.config.max_frame_size)?;

        // Clear pending only after all operations succeed so a failed flush
        // leaves the queue intact for retry or inspection.
        self.pending.clear();

        debug!(
            channel_id = ?frame.id,
            frame_number = frame.number,
            is_last = frame.is_last,
            data_len = frame.data.len(),
            "flushed channel to frame"
        );

        Ok(vec![frame])
    }
}

#[cfg(test)]
mod tests {
    use base_consensus_genesis::HardForkConfig;
    use base_protocol::{Batch, BatchReader, SingleBatch};

    use super::*;

    fn driver() -> ChannelDriver {
        ChannelDriver::new(RollupConfig::default(), ChannelDriverConfig::default())
    }

    #[test]
    fn flush_empty_returns_error() {
        let mut d = driver();
        assert!(matches!(d.flush(), Err(ChannelDriverError::Empty)));
    }

    #[test]
    fn flush_single_batch_produces_one_frame() {
        let mut d = driver();
        d.add_batch(SingleBatch { timestamp: 100, ..Default::default() });
        let frames = d.flush().unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_last);
        assert!(!frames[0].data.is_empty());
    }

    #[test]
    fn flush_drains_pending_batches() {
        let mut d = driver();
        d.add_batch(SingleBatch::default());
        d.add_batch(SingleBatch::default());
        assert_eq!(d.pending_count(), 2);
        d.flush().unwrap();
        assert_eq!(d.pending_count(), 0);
        assert!(d.is_empty());
    }

    #[test]
    fn flush_twice_encodes_independently() {
        let mut d = driver();
        d.add_batch(SingleBatch { timestamp: 10, ..Default::default() });
        let frames_a = d.flush().unwrap();

        d.add_batch(SingleBatch { timestamp: 20, ..Default::default() });
        let frames_b = d.flush().unwrap();

        // Each flush produces an independent, valid frame.
        // Both share the same channel ID ([0u8; 16]) because ChannelId::default()
        // is passed to ChannelOut::new each time; ChannelOut does not randomise it.
        assert_eq!(frames_a.len(), 1);
        assert_eq!(frames_b.len(), 1);
        assert!(frames_a[0].is_last);
        assert!(frames_b[0].is_last);
        assert_eq!(frames_a[0].id, frames_b[0].id);
    }

    #[test]
    fn flush_and_decode_roundtrip() {
        let rollup_config = RollupConfig {
            hardforks: HardForkConfig { fjord_time: Some(0), ..Default::default() },
            ..Default::default()
        };
        let mut d = ChannelDriver::new(rollup_config.clone(), ChannelDriverConfig::default());
        d.add_batch(SingleBatch { timestamp: 2, ..Default::default() });
        let frames = d.flush().unwrap();
        assert_eq!(frames.len(), 1);

        // Simulate channel assembly: channel data is the concatenation of frame data.
        let channel_data = frames[0].data.clone();
        let max_rlp = 10_000_000usize;
        let mut reader = BatchReader::new(channel_data, max_rlp);
        reader.decompress().expect("decompression should succeed");
        let decoded = reader.next_batch(&rollup_config).expect("should decode batch");
        match decoded {
            Batch::Single(sb) => assert_eq!(sb.timestamp, 2),
            _ => panic!("expected single batch"),
        }
    }

    #[test]
    fn multiple_batches_compress_into_one_frame() {
        let mut d = driver();
        for i in 0..5 {
            d.add_batch(SingleBatch { timestamp: i * 2, ..Default::default() });
        }
        let frames = d.flush().unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_last);
    }
}
