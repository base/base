//! Contains the `ChannelOut` primitive for Base.

use alloc::{sync::Arc, vec, vec::Vec};

use alloy_rlp::Encodable;
use base_common_genesis::RollupConfig;
use base_protocol::{BatchType, ChannelId, Frame, SingleBatch};

use crate::{CompressorError, CompressorWriter};

/// An error returned while building or framing a [`ChannelOut`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelOutError {
    /// The max frame size is too small.
    #[error("max frame size is too small")]
    MaxFrameSizeTooSmall,
    /// An error from compression.
    #[error("compression failed: {0}")]
    Compression(#[from] CompressorError),
    /// The encoded batch exceeds the max RLP bytes per channel.
    #[error("encoded batch exceeds the max RLP bytes per channel")]
    ExceedsMaxRlpBytesPerChannel,
}

/// Owns compression and final framing for one derivation channel.
///
/// The Single producer writes batches through [`Self::add_single_batch`]. The
/// Span producer supplies an already-populated compressor and uses only
/// [`Self::into_frames`] for the terminal framing step.
#[derive(derive_more::Debug)]
pub struct ChannelOut<C>
where
    C: CompressorWriter,
{
    /// The unique identifier for the channel.
    id: ChannelId,
    /// The [`RollupConfig`] used to check the max RLP bytes per channel when
    /// encoding and accepting batches.
    #[debug(skip)]
    config: Arc<RollupConfig>,
    /// Total RLP input bytes accepted by the Single producer.
    rlp_length: u64,
    /// The compressor.
    #[debug(skip)]
    compressor: C,
}

impl<C> ChannelOut<C>
where
    C: CompressorWriter,
{
    /// Wraps the compressor used to build or finalize the identified channel.
    pub const fn new(id: ChannelId, config: Arc<RollupConfig>, compressor: C) -> Self {
        Self { id, config, rlp_length: 0, compressor }
    }

    /// Returns the channel identifier.
    pub const fn id(&self) -> ChannelId {
        self.id
    }

    /// Encodes and compresses one batch for the Single producer path.
    ///
    /// The Single variant of the encoder's open channel calls this once per
    /// accepted L2 block. The write is rejected before compression if it would
    /// exceed the protocol's cumulative RLP limit.
    pub fn add_single_batch(&mut self, batch: SingleBatch) -> Result<(), ChannelOutError> {
        let timestamp = batch.timestamp;

        // Build the derivation wire payload: batch type followed by SingleBatch RLP.
        let mut buf = vec![BatchType::Single as u8];
        batch.encode(&mut buf);

        // Wrap in an RLP byte string so the BatchReader can decode it via Bytes::decode().
        // Use `&buf[..]` (a `[u8]` slice) to get the byte-string encoding rather than
        // `buf` (a `Vec<u8>`) which would use the generic Vec<T> list encoding.
        let mut rlp_buf = vec![];
        buf.as_slice().encode(&mut rlp_buf);

        // Validate that the RLP length is within the channel's limits.
        let max_rlp_bytes_per_channel = self.config.max_rlp_bytes_per_channel(timestamp);
        if self.rlp_length + rlp_buf.len() as u64 > max_rlp_bytes_per_channel {
            return Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel);
        }

        self.compressor.write(&rlp_buf)?;
        self.rlp_length += rlp_buf.len() as u64;

        Ok(())
    }

    /// Returns the cumulative RLP input used for limits and encoder metrics.
    pub const fn input_bytes(&self) -> u64 {
        self.rlp_length
    }

    /// Consumes the channel and returns its complete ordered frame list.
    ///
    /// The encoder calls this when an open channel closes. It finalizes the
    /// compressor, emits the compression-version byte in the first frame, and
    /// marks only the final frame as `is_last`.
    pub fn into_frames(mut self, max_size: usize) -> Result<Vec<Frame>, ChannelOutError> {
        let mut frames = Vec::new();
        let mut frame_number = 0;
        loop {
            let remaining = self.compressor.compressed_len()?;
            if remaining == 0 {
                break;
            }

            // Brotli's channel-version byte is emitted once in the first frame;
            // zlib data is self-identifying.
            let version_byte =
                if frame_number == 0 { self.compressor.channel_version_byte() } else { None };
            let prefix_len = usize::from(version_byte.is_some());
            let overhead = Frame::ENCODED_OVERHEAD + prefix_len;
            if max_size <= overhead {
                return Err(ChannelOutError::MaxFrameSizeTooSmall);
            }

            let payload_size = (max_size - overhead).min(remaining);
            let mut data = Vec::with_capacity(prefix_len + payload_size);
            if let Some(version) = version_byte {
                data.push(version);
            }

            let payload_start = data.len();
            data.resize(payload_start + payload_size, 0);
            self.compressor.read(&mut data[payload_start..])?;

            let frame = Frame {
                id: self.id,
                number: frame_number,
                is_last: self.compressor.compressed_len()? == 0,
                data,
            };
            frame_number += 1;
            frames.push(frame);
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};

    use alloy_primitives::Bytes;
    use base_protocol::SingleBatch;

    use super::*;
    use crate::test_utils::MockCompressor;

    #[test]
    fn into_frames_rejects_too_small_max_size() {
        let compressor =
            MockCompressor { compressed: Some(Bytes::from_static(b"x")), ..Default::default() };
        let channel =
            ChannelOut::new(ChannelId::default(), Arc::new(RollupConfig::default()), compressor);

        assert_eq!(
            channel.into_frames(Frame::ENCODED_OVERHEAD),
            Err(ChannelOutError::MaxFrameSizeTooSmall)
        );
    }

    #[test]
    fn into_frames_reserves_channel_version_byte() {
        let compressor = MockCompressor {
            compressed: Some(Bytes::from_static(b"abc")),
            version_byte: Some(1),
            ..Default::default()
        };
        let channel =
            ChannelOut::new(ChannelId::default(), Arc::new(RollupConfig::default()), compressor);

        let frames = channel
            .into_frames(Frame::ENCODED_OVERHEAD + 2)
            .expect("version and payload bytes should fit");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].number, 0);
        assert!(!frames[0].is_last);
        assert_eq!(frames[0].data, Bytes::from_static(b"\x01a"));
        assert_eq!(frames[1].number, 1);
        assert!(frames[1].is_last);
        assert_eq!(frames[1].data, Bytes::from_static(b"bc"));
    }

    #[test]
    fn into_frames_propagates_compressor_error() {
        let channel = ChannelOut::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            MockCompressor {
                compressed: Some(Bytes::from_static(b"x")),
                read_error: true,
                ..Default::default()
            },
        );

        let err = channel.into_frames(Frame::ENCODED_OVERHEAD + 1).unwrap_err();
        assert_eq!(err, ChannelOutError::Compression(CompressorError::Full));
    }

    #[test]
    fn into_frames_marks_only_frame_last() {
        let channel = ChannelOut::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            MockCompressor { compressed: Some(Bytes::from_static(b"x")), ..Default::default() },
        );
        let frames = channel.into_frames(Frame::ENCODED_OVERHEAD + 1).unwrap();
        let frame = &frames[0];
        assert_eq!(frame.id, ChannelId::default());
        assert_eq!(frame.number, 0);
        assert!(frame.is_last);
    }

    #[test]
    fn test_channel_out_max_rlp_bytes_per_channel() {
        let mut channel = ChannelOut::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            MockCompressor::default(),
        );

        let batch = SingleBatch::default();
        channel.rlp_length = channel.config.max_rlp_bytes_per_channel(batch.timestamp);

        assert_eq!(
            channel.add_single_batch(batch),
            Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel)
        );
    }

    #[test]
    fn test_channel_out_add_single_batch() {
        let config = RollupConfig::default();
        let mut channel =
            ChannelOut::new(ChannelId::default(), Arc::new(config), MockCompressor::default());

        assert_eq!(channel.add_single_batch(SingleBatch::default()), Ok(()));
    }

    #[test]
    fn test_channel_out_add_single_batch_enforces_cumulative_rlp_limit() {
        let mut channel = ChannelOut::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            MockCompressor::default(),
        );

        let timestamp = 0;
        let max_rlp = channel.config.max_rlp_bytes_per_channel(timestamp);
        let payload_size = (max_rlp / 2 + 1) as usize;

        let large_batch = SingleBatch {
            timestamp,
            transactions: vec![Bytes::from(vec![0u8; payload_size])],
            ..Default::default()
        };

        let mut encoded = vec![BatchType::Single as u8];
        large_batch.encode(&mut encoded);
        assert!(encoded.len() as u64 <= max_rlp, "test batch should fit within per-channel limit");

        channel.add_single_batch(large_batch.clone()).expect("first batch should fit");
        // rlp_length tracks the RLP byte-string-wrapped size (includes header bytes).
        let mut rlp_wrapped = Vec::new();
        encoded.as_slice().encode(&mut rlp_wrapped);
        assert_eq!(channel.rlp_length, rlp_wrapped.len() as u64);

        let err = channel.add_single_batch(large_batch).unwrap_err();
        assert_eq!(err, ChannelOutError::ExceedsMaxRlpBytesPerChannel);
    }
}
