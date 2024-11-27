//! Contains the `ChannelOut` primitive for Optimism.

use crate::{Batch, ChannelId, Compressor, Frame};
use alloc::vec;
use alloy_primitives::Bytes;
use op_alloy_genesis::RollupConfig;

/// The frame overhead.
const FRAME_V0_OVERHEAD: usize = 23;

/// An error returned by the [ChannelOut] when adding single batches.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ChannelOutError {
    /// The channel is closed.
    #[error("The channel is already closed")]
    ChannelClosed,
    /// The max frame size is too small.
    #[error("The max frame size is too small")]
    MaxFrameSizeTooSmall,
    /// Missing compressed batch data.
    #[error("Missing compressed batch data")]
    MissingData,
    /// An error from brotli compression.
    #[error("Error from Brotli compression")]
    BrotliCompression,
    /// An error encoding the `Batch`.
    #[error("Error encoding the batch")]
    BatchEncoding,
    /// The encoded batch exceeds the max RLP bytes per channel.
    #[error("The encoded batch exceeds the max RLP bytes per channel")]
    ExceedsMaxRlpBytesPerChannel,
}

/// [ChannelOut] constructs a channel from compressed, encoded batch data.
#[derive(Debug, Clone)]
pub struct ChannelOut<'a, C>
where
    C: Compressor + Clone + core::fmt::Debug,
{
    /// The unique identifier for the channel.
    pub id: ChannelId,
    /// A reference to the [RollupConfig] used to
    /// check the max RLP bytes per channel when
    /// encoding and accepting the
    pub config: &'a RollupConfig,
    /// The rlp length of the channel.
    pub rlp_length: u64,
    /// Whether the channel is closed.
    pub closed: bool,
    /// The frame number.
    pub frame_number: u16,
    /// Compressed batch data.
    pub compressed: Option<Bytes>,
    /// The compressor.
    pub compressor: C,
}

impl<'a, C> ChannelOut<'a, C>
where
    C: Compressor + Clone + core::fmt::Debug,
{
    /// Creates a new [ChannelOut] with the given [ChannelId].
    pub const fn new(id: ChannelId, config: &'a RollupConfig, compressor: C) -> Self {
        Self {
            id,
            config,
            rlp_length: 0,
            frame_number: 0,
            closed: false,
            compressed: None,
            compressor,
        }
    }

    /// Accepts the given [crate::Batch] data into the [ChannelOut], compressing it
    /// into frames.
    pub fn add_batch(&mut self, batch: Batch) -> Result<(), ChannelOutError> {
        if self.closed {
            return Err(ChannelOutError::ChannelClosed);
        }

        // Encode the batch.
        let mut buf = vec![];
        batch.encode(&mut buf).map_err(|_| ChannelOutError::BatchEncoding)?;

        // Validate that the RLP length is within the channel's limits.
        let max_rlp_bytes_per_channel = self.config.max_rlp_bytes_per_channel(batch.timestamp());
        if self.rlp_length + buf.len() as u64 > max_rlp_bytes_per_channel {
            return Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel);
        }

        self.compressed = Some(self.compressor.compress(&buf).into());

        Ok(())
    }

    /// Returns the number of bytes ready to be output to a frame.
    pub fn ready_bytes(&self) -> usize {
        self.compressed.as_ref().map_or(0, |c| c.len())
    }

    /// Closes the channel if not already closed.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Outputs a [Frame] from the [ChannelOut].
    pub fn output_frame(&mut self, max_size: usize) -> Result<Frame, ChannelOutError> {
        if max_size < FRAME_V0_OVERHEAD {
            return Err(ChannelOutError::MaxFrameSizeTooSmall);
        }

        // Construct an empty frame.
        let mut frame =
            Frame { id: self.id, number: self.frame_number, is_last: self.closed, data: vec![] };

        let mut max_size = max_size - FRAME_V0_OVERHEAD;
        if max_size > self.ready_bytes() {
            max_size = self.ready_bytes();
        }

        // Read `max_size` bytes from the compressed data.
        let data = if let Some(data) = &self.compressed {
            &data[..max_size]
        } else {
            return Err(ChannelOutError::MissingData);
        };
        frame.data.extend_from_slice(data);

        // Update the compressed data.
        self.compressed = self.compressed.as_mut().map(|b| b.split_off(max_size));
        self.frame_number += 1;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SingleBatch, SpanBatch};

    #[derive(Debug, Clone)]
    struct MockCompressor;

    impl Compressor for MockCompressor {
        fn compress(&self, data: &[u8]) -> Vec<u8> {
            data.to_vec()
        }
    }

    #[test]
    fn test_channel_out_ready_bytes_empty() {
        let config = RollupConfig::default();
        let channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);
        assert_eq!(channel.ready_bytes(), 0);
    }

    #[test]
    fn test_channel_out_ready_bytes_some() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);
        channel.compressed = Some(Bytes::from(vec![1, 2, 3]));
        assert_eq!(channel.ready_bytes(), 3);
    }

    #[test]
    fn test_channel_out_close() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);
        assert!(!channel.closed);

        channel.close();
        assert!(channel.closed);
    }

    #[test]
    fn test_channel_out_add_batch_closed() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);
        channel.close();

        let batch = Batch::Single(SingleBatch::default());
        assert_eq!(channel.add_batch(batch), Err(ChannelOutError::ChannelClosed));
    }

    #[test]
    fn test_channel_out_empty_span_batch_decode_error() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);

        let batch = Batch::Span(SpanBatch::default());
        assert_eq!(channel.add_batch(batch), Err(ChannelOutError::BatchEncoding));
    }

    #[test]
    fn test_channel_out_max_rlp_bytes_per_channel() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);

        let batch = Batch::Single(SingleBatch::default());
        channel.rlp_length = config.max_rlp_bytes_per_channel(batch.timestamp());

        assert_eq!(channel.add_batch(batch), Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel));
    }

    #[test]
    fn test_channel_out_add_batch() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor);

        let batch = Batch::Single(SingleBatch::default());
        assert_eq!(channel.add_batch(batch), Ok(()));
    }
}
