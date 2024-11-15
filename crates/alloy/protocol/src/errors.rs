//! Error types for protocol conversions.

use crate::DecodeError;
use alloy_primitives::B256;

/// An error encountered during [OpBlock] conversion.
///
/// [OpBlock]: op_alloy_consensus::OpBlock
#[derive(Debug, derive_more::Display)]
pub enum OpBlockConversionError {
    /// Invalid genesis hash.
    #[display("Invalid genesis hash. Expected {_0}, got {_1}")]
    InvalidGenesisHash(B256, B256),
    /// Invalid transaction type.
    #[display("First payload transaction has unexpected type: {_0}")]
    InvalidTxType(u8),
    /// L1 Info error
    #[display("Failed to decode L1 info: {_0}")]
    L1InfoError(DecodeError),
    /// Missing system config in genesis block.
    #[display("Missing system config in genesis block")]
    MissingSystemConfigGenesis,
    /// Empty transactions.
    #[display("Empty transactions in payload. Block hash: {_0}")]
    EmptyTransactions(B256),
    /// EIP-1559 parameter decoding error.
    #[display("Failed to decode EIP-1559 parameters from header's `nonce` field.")]
    Eip1559DecodeError,
}

impl core::error::Error for OpBlockConversionError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::L1InfoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<DecodeError> for OpBlockConversionError {
    fn from(e: DecodeError) -> Self {
        Self::L1InfoError(e)
    }
}
