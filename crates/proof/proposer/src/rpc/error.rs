//! RPC-specific error types.

use alloy_transport::TransportError;
use thiserror::Error;

/// RPC-specific error type.
#[derive(Debug, Error)]
pub enum RpcError {
    /// Transport error from alloy.
    #[error("Transport error: {0}")]
    Transport(String),

    /// Block not found.
    #[error("Block not found: {0}")]
    BlockNotFound(String),

    /// Header not found.
    #[error("Header not found: {0}")]
    HeaderNotFound(String),

    /// Proof not found.
    #[error("Proof not found: {0}")]
    ProofNotFound(String),

    /// Witness not found.
    #[error("Witness not found: {0}")]
    WitnessNotFound(String),

    /// Invalid response from RPC.
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Request timeout.
    #[error("Request timeout: {0}")]
    Timeout(String),

    /// Connection error.
    #[error("Connection error: {0}")]
    Connection(String),

    /// Serialization/deserialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Header chain validation failed.
    #[error("Header chain validation failed: {0}")]
    HeaderChainInvalid(String),
}

impl RpcError {
    /// Returns true if this error is transient and the operation should be retried.
    ///
    /// Only transport-level errors (network issues, timeouts, connection failures)
    /// are considered retryable. Application-level errors (not found, invalid data)
    /// are not retryable.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Timeout(_) | Self::Connection(_))
    }
}

impl From<TransportError> for RpcError {
    fn from(err: TransportError) -> Self {
        Self::Transport(err.to_string())
    }
}

impl From<serde_json::Error> for RpcError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

/// Result type alias for RPC operations.
pub type RpcResult<T> = Result<T, RpcError>;
