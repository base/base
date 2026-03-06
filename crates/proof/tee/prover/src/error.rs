use base_enclave_client::ClientError;
use base_proof_rpc::RpcError;

/// Errors from shared TEE proving operations.
#[derive(Debug, thiserror::Error)]
pub enum TeeProverError {
    /// Transaction serialization failed.
    #[error("serialization failed: {0}")]
    Serialization(String),

    /// Proof encoding failed.
    #[error("encoding failed: {0}")]
    Encoding(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),
}

/// Errors from range-based TEE proving.
#[derive(Debug, thiserror::Error)]
pub enum RangeProverError {
    /// RPC data fetch failed.
    #[error("failed to fetch {context}: {source}")]
    Rpc {
        /// Description of the fetch operation.
        context: &'static str,
        /// The underlying RPC error.
        source: RpcError,
    },

    /// Enclave execution failed.
    #[error("enclave execution failed: {0}")]
    Enclave(ClientError),

    /// Enclave execution timed out.
    #[error("enclave execution timed out")]
    Timeout,

    /// Non-RPC data preparation failed (serialization, derivation, arithmetic overflow).
    #[error("data preparation failed: {0}")]
    DataPrep(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),

    /// No blocks to prove.
    #[error("no blocks to prove")]
    Empty,
}

impl RangeProverError {
    /// Returns `true` if the error is transient and the operation can be retried.
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Rpc { source, .. } => source.is_retryable(),
            Self::Enclave(e) => e.is_retryable(),
            Self::Timeout | Self::DataPrep(_) | Self::Config(_) | Self::Empty => false,
        }
    }
}
