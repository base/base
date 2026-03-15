//! Error types for the remote signer.

use thiserror::Error;

/// Errors that can occur during remote signing operations.
#[derive(Debug, Error)]
pub enum RemoteSignerError {
    /// An error occurred during RPC transport.
    #[error("transport error: {0}")]
    Transport(#[from] alloy_transport::TransportError),
    /// Failed to decode the signed transaction bytes returned by the signer.
    #[error("failed to decode signed transaction: {0}")]
    Decode(String),
}
