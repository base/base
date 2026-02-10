//! Error types for the proposer.

use thiserror::Error;

use crate::rpc::RpcError;

/// Main error type for the proposer.
#[derive(Debug, Error)]
pub enum ProposerError {
    /// RPC connection error.
    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError),

    /// Enclave communication error.
    #[error("Enclave error: {0}")]
    Enclave(String),

    /// Contract interaction error.
    #[error("Contract error: {0}")]
    Contract(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Internal error.
    #[error("Internal error: {0}")]
    Internal(String),

    /// Output root mismatch between enclave and local computation.
    #[error("output root mismatch: expected {expected}, got {actual}")]
    OutputRootMismatch {
        /// Expected output root.
        expected: alloy_primitives::B256,
        /// Actual output root from enclave.
        actual: alloy_primitives::B256,
    },

    /// L1 origin hash mismatch.
    #[error("L1 origin mismatch: expected {expected}, got {actual}")]
    L1OriginMismatch {
        /// Expected L1 origin hash.
        expected: alloy_primitives::B256,
        /// Actual L1 origin hash.
        actual: alloy_primitives::B256,
    },

    /// Block number mismatch.
    #[error("block number mismatch: expected {expected}, got {actual}")]
    BlockNumberMismatch {
        /// Expected block number.
        expected: u64,
        /// Actual block number.
        actual: u64,
    },

    /// Failed to derive block info.
    #[error("failed to derive block info: {0}")]
    BlockInfoDerivation(String),

    /// Failed to serialize transaction.
    #[error("failed to serialize transaction: {0}")]
    TxSerialization(String),
}

impl From<eyre::Error> for ProposerError {
    fn from(err: eyre::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

/// Result type alias for proposer operations.
pub type ProposerResult<T> = Result<T, ProposerError>;
