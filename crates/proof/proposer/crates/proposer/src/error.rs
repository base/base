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
}

impl From<eyre::Error> for ProposerError {
    fn from(err: eyre::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

/// Result type alias for proposer operations.
pub type ProposerResult<T> = Result<T, ProposerError>;
