//! Error types for the remote signer.

use alloy_primitives::Address;
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
    /// Failed to recover the signer address from the signature.
    #[error("failed to recover signer address: {0}")]
    Recovery(String),
    /// The recovered signer address does not match the expected address.
    #[error("signer mismatch: expected {expected}, got {recovered}")]
    SignerMismatch {
        /// The expected signer address.
        expected: Address,
        /// The recovered signer address.
        recovered: Address,
    },
}
