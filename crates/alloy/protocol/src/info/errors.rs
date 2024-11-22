//! Contains error types specific to the L1 block info transaction.

use alloc::string::String;

/// An error type for parsing L1 block info transactions.
#[derive(Debug, thiserror::Error, Copy, Clone)]
pub enum BlockInfoError {
    /// Failed to parse the L1 blob base fee scalar.
    L1BlobBaseFeeScalar,
    /// Failed to parse the base fee scalar.
    BaseFeeScalar,
    /// Failed to parse the EIP-1559 denominator.
    Eip1559Denominator,
    /// Failed to parse the EIP-1559 elasticity parameter.
    Eip1559Elasticity,
}

impl core::fmt::Display for BlockInfoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::L1BlobBaseFeeScalar => {
                write!(f, "Failed to parse the L1 blob base fee scalar")
            }
            Self::BaseFeeScalar => write!(f, "Failed to parse the base fee scalar"),
            Self::Eip1559Denominator => {
                write!(f, "Failed to parse the EIP-1559 denominator")
            }
            Self::Eip1559Elasticity => {
                write!(f, "Failed to parse the EIP-1559 elasticity parameter")
            }
        }
    }
}

/// An error decoding an L1 block info transaction.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// Invalid selector for the L1 info transaction
    InvalidSelector,
    /// Parse error for the L1 info transaction
    ParseError(String),
    /// Invalid length for the L1 info transaction
    InvalidLength(String),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidSelector => write!(f, "Invalid L1 info transaction selector"),
            Self::ParseError(msg) => write!(f, "Parse error: {}", msg),
            Self::InvalidLength(msg) => write!(f, "Invalid data length: {}", msg), /* Handle display for length errors */
        }
    }
}
