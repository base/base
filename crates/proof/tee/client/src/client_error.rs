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
