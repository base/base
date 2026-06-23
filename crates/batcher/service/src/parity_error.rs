//! Parity normalization errors.

use base_blobs::BlobDecodeError;
use base_protocol::{BatchDecodingError, BatchReaderError, DecompressionError, FrameParseError};

/// Error returned while normalizing a batcher DA submission.
#[derive(Debug, thiserror::Error)]
pub enum ParityError {
    /// The blob payload failed Base blob decoding.
    #[error("failed to decode blob payload: {0}")]
    BlobDecode(#[from] BlobDecodeError),
    /// The decoded payload failed batcher frame parsing.
    #[error("failed to parse batcher frames: {0}")]
    FrameParse(#[from] FrameParseError),
    /// The complete channel failed decompression.
    #[error("failed to decompress channel: {0}")]
    ChannelDecompress(#[from] DecompressionError),
    /// The decompressed channel failed RLP payload decoding.
    #[error("failed to decode channel RLP payload: {0}")]
    ChannelRlp(#[from] alloy_rlp::Error),
    /// A decompressed channel payload failed batch decoding.
    #[error("failed to decode batch: {0}")]
    BatchDecode(#[from] BatchDecodingError),
}

impl From<BatchReaderError> for ParityError {
    fn from(error: BatchReaderError) -> Self {
        match error {
            BatchReaderError::Decompression(error) => Self::ChannelDecompress(error),
            BatchReaderError::Rlp(error) => Self::ChannelRlp(error),
            BatchReaderError::Batch(error) => Self::BatchDecode(error),
        }
    }
}
