//! Client error types.

use thiserror::Error;

/// Errors that can occur when using the enclave client.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Failed to create the HTTP client.
    #[error("failed to create client: {0}")]
    ClientCreation(String),

    /// RPC call failed.
    #[error("RPC error: {0}")]
    Rpc(#[from] jsonrpsee::core::client::Error),

    /// Invalid URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

impl ClientError {
    /// Returns `true` if this error is transient and the operation can be retried.
    ///
    /// Only transport-level failures from the underlying jsonrpsee client are
    /// considered retryable. Configuration errors (`ClientCreation`, `InvalidUrl`)
    /// are never retryable.
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Rpc(
                jsonrpsee::core::client::Error::Transport(_)
                    | jsonrpsee::core::client::Error::RestartNeeded(_)
                    | jsonrpsee::core::client::Error::RequestTimeout
                    | jsonrpsee::core::client::Error::ServiceDisconnect,
            )
        )
    }
}
