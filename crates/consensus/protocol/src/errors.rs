//! Error types for protocol conversions.

use alloc::string::String;

use alloy_primitives::B256;
use base_common_consensus::EIP1559ParamError;

use crate::DecodeError;

/// An error encountered during Base [`Block`](alloy_consensus::Block) or payload conversion.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum BaseBlockConversionError {
    /// Invalid genesis hash.
    #[error("Invalid genesis hash. Expected {0}, got {1}")]
    InvalidGenesisHash(B256, B256),
    /// Invalid transaction type.
    #[error("First payload transaction has unexpected type: {0}")]
    InvalidTxType(u8),
    /// Invalid transaction encoding.
    #[error("Failed to decode first payload transaction: {0}")]
    InvalidTransactionEncoding(String),
    /// L1 Info error
    #[error("Failed to decode L1 info: {0}")]
    L1InfoError(#[from] DecodeError),
    /// Missing system config in genesis block.
    #[error("Missing system config in genesis block")]
    MissingSystemConfigGenesis,
    /// Empty transactions.
    #[error("Empty transactions in payload. Block hash: {0}")]
    EmptyTransactions(B256),
    /// EIP-1559 parameter decoding error.
    #[error("Failed to decode EIP-1559 parameters from header's `extraData` field.")]
    Eip1559DecodeError(#[from] EIP1559ParamError),
}
