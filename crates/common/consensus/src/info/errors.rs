//! Contains error types specific to the L1 block info transaction.

/// An error type for parsing L1 block info transactions.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Copy, Clone)]
pub enum BlockInfoError {
    /// Failed to parse the L1 blob base fee scalar.
    #[error("Failed to parse the L1 blob base fee scalar")]
    L1BlobBaseFeeScalar,
    /// Failed to parse the base fee scalar.
    #[error("Failed to parse the base fee scalar")]
    BaseFeeScalar,
}

/// An error decoding an L1 block info transaction.
#[derive(Debug, Eq, PartialEq, Clone, thiserror::Error)]
pub enum DecodeError {
    /// Missing selector bytes.
    #[error("The provided calldata is too short, missing the 4 selector bytes")]
    MissingSelector,
    /// Invalid selector for the L1 info transaction.
    #[error("Invalid L1 info transaction selector")]
    InvalidSelector,
    /// Invalid length for the L1 info bedrock transaction.
    /// Arguments are the expected length and the actual length.
    #[error("Invalid bedrock data length. Expected {0}, got {1}")]
    InvalidBedrockLength(usize, usize),
    /// Invalid length for the L1 info ecotone transaction.
    /// Arguments are the expected length and the actual length.
    #[error("Invalid ecotone data length. Expected {0}, got {1}")]
    InvalidEcotoneLength(usize, usize),
    /// Invalid length for the L1 info isthmus transaction.
    /// Arguments are the expected length and the actual length.
    #[error("Invalid isthmus data length. Expected {0}, got {1}")]
    InvalidIsthmusLength(usize, usize),
    /// Invalid length for the L1 info jovian transaction.
    /// Arguments are the expected length and the actual length.
    #[error("Invalid jovian data length. Expected {0}, got {1}")]
    InvalidJovianLength(usize, usize),
}
