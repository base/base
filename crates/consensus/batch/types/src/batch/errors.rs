//! Errors decoding singular batches.

/// An error decoding a batch.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BatchDecodingError {
    /// Empty buffer.
    #[error("Empty buffer")]
    EmptyBuffer,
    /// Invalid RLP payload.
    #[error("Error decoding an Alloy RLP: {0}")]
    AlloyRlpError(alloy_rlp::Error),
    /// Unsupported batch type, including the retired span format (1).
    #[error("Invalid batch type: {0}")]
    InvalidBatchType(u8),
}
