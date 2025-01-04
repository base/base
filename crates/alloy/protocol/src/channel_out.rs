//! Contains the `ChannelOut` primitive for Optimism.

use crate::{Batch, ChannelCompressor, ChannelId, CompressorError, Frame};
use alloc::{vec, vec::Vec};
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
    /// An error from compression.
    #[error("Error from compression")]
    Compression(#[from] CompressorError),
    /// An error encoding the `Batch`.
    #[error("Error encoding the batch")]
    BatchEncoding,
    /// The encoded batch exceeds the max RLP bytes per channel.
    #[error("The encoded batch exceeds the max RLP bytes per channel")]
    ExceedsMaxRlpBytesPerChannel,
}

/// [ChannelOut] constructs a channel from compressed, encoded batch data.
#[allow(missing_debug_implementations)]
pub struct ChannelOut<'a, C>
where
    C: ChannelCompressor,
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
    /// The compressor.
    pub compressor: C,
}

impl<'a, C> ChannelOut<'a, C>
where
    C: ChannelCompressor,
{
    /// Creates a new [ChannelOut] with the given [ChannelId].
    pub const fn new(id: ChannelId, config: &'a RollupConfig, compressor: C) -> Self {
        Self { id, config, rlp_length: 0, frame_number: 0, closed: false, compressor }
    }

    /// Resets the [ChannelOut] to its initial state.
    pub fn reset(&mut self) {
        self.rlp_length = 0;
        self.frame_number = 0;
        self.closed = false;
        self.compressor.reset();
        // TODO: read random bytes into the channel id.
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

        self.compressor.write(&buf)?;

        Ok(())
    }

    /// Returns the total amount of rlp-encoded input bytes.
    pub const fn input_bytes(&self) -> u64 {
        self.rlp_length
    }

    /// Returns the number of bytes ready to be output to a frame.
    pub fn ready_bytes(&self) -> usize {
        self.compressor.len()
    }

    /// Flush the internal compressor.
    pub fn flush(&mut self) -> Result<(), ChannelOutError> {
        self.compressor.flush()?;
        Ok(())
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
        let mut data = Vec::with_capacity(max_size);
        self.compressor.read(&mut data).map_err(ChannelOutError::Compression)?;
        frame.data.extend_from_slice(data.as_slice());

        // Update the compressed data.
        self.frame_number += 1;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompressorResult, CompressorWriter, SingleBatch, SpanBatch};
    use alloy_primitives::Bytes;

    #[derive(Debug, Clone, Default)]
    struct MockCompressor {
        pub compressed: Option<Bytes>,
    }

    impl CompressorWriter for MockCompressor {
        fn write(&mut self, data: &[u8]) -> CompressorResult<usize> {
            let data = data.to_vec();
            let written = data.len();
            self.compressed = Some(Bytes::from(data));
            Ok(written)
        }

        fn flush(&mut self) -> CompressorResult<()> {
            Ok(())
        }

        fn close(&mut self) -> CompressorResult<()> {
            Ok(())
        }

        fn reset(&mut self) {
            self.compressed = None;
        }

        fn len(&self) -> usize {
            self.compressed.as_ref().map(|b| b.len()).unwrap_or(0)
        }

        fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
            let len = self.compressed.as_ref().map(|b| b.len()).unwrap_or(0);
            buf[..len].copy_from_slice(self.compressed.as_ref().unwrap());
            Ok(len)
        }
    }

    impl ChannelCompressor for MockCompressor {
        fn get_compressed(&self) -> Vec<u8> {
            self.compressed.as_ref().unwrap().to_vec()
        }
    }

    #[test]
    fn test_channel_out_ready_bytes_empty() {
        let config = RollupConfig::default();
        let channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());
        assert_eq!(channel.ready_bytes(), 0);
    }

    #[test]
    fn test_channel_out_ready_bytes_some() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());
        channel.compressor.write(&[1, 2, 3]).unwrap();
        assert_eq!(channel.ready_bytes(), 3);
    }

    #[test]
    fn test_channel_out_close() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());
        assert!(!channel.closed);

        channel.close();
        assert!(channel.closed);
    }

    #[test]
    fn test_channel_out_add_batch_closed() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());
        channel.close();

        let batch = Batch::Single(SingleBatch::default());
        assert_eq!(channel.add_batch(batch), Err(ChannelOutError::ChannelClosed));
    }

    #[test]
    fn test_channel_out_empty_span_batch_decode_error() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());

        let batch = Batch::Span(SpanBatch::default());
        assert_eq!(channel.add_batch(batch), Err(ChannelOutError::BatchEncoding));
    }

    #[test]
    fn test_channel_out_max_rlp_bytes_per_channel() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());

        let batch = Batch::Single(SingleBatch::default());
        channel.rlp_length = config.max_rlp_bytes_per_channel(batch.timestamp());

        assert_eq!(channel.add_batch(batch), Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel));
    }

    #[test]
    fn test_channel_out_add_batch() {
        let config = RollupConfig::default();
        let mut channel = ChannelOut::new(ChannelId::default(), &config, MockCompressor::default());

        let batch = Batch::Single(SingleBatch::default());
        assert_eq!(channel.add_batch(batch), Ok(()));
    }
}
