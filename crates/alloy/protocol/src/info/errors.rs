//! Contains error types specific to the L1 block info transaction.

use alloc::string::String;

/// An error type for parsing L1 block info transactions.
#[derive(Debug, thiserror::Error, Copy, Clone)]
pub enum BlockInfoError {
    /// Failed to parse the L1 blob base fee scalar.
    #[error("Failed to parse the L1 blob base fee scalar")]
    L1BlobBaseFeeScalar,
    /// Failed to parse the base fee scalar.
    #[error("Failed to parse the base fee scalar")]
    BaseFeeScalar,
    /// Failed to parse the EIP-1559 denominator.
    #[error("Failed to parse the EIP-1559 denominator")]
    Eip1559Denominator,
    /// Failed to parse the EIP-1559 elasticity parameter.
    #[error("Failed to parse the EIP-1559 elasticity parameter")]
    Eip1559Elasticity,
}

/// An error decoding an L1 block info transaction.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Invalid selector for the L1 info transaction.
    #[error("Invalid L1 info transaction selector")]
    InvalidSelector,
    /// Parse error for the L1 info transaction.
    #[error("Parse error: {0}")]
    ParseError(String),
    /// Invalid length for the L1 info transaction.
    #[error("Invalid data length: {0}")]
    InvalidLength(String),
}
