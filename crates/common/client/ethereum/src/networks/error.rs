//! Error types for the remote signer.

use alloy_primitives::{Address, B256};
use thiserror::Error;

/// Errors that can occur during remote signing operations.
#[derive(Debug, Error)]
pub enum RemoteSignerError {
    /// An error occurred during the JSON-RPC call.
    #[error("rpc error: {0}")]
    Rpc(#[source] jsonrpsee::core::ClientError),
    /// Failed to build the JSON-RPC HTTP client.
    #[error("client build error: {0}")]
    Client(#[source] jsonrpsee::core::ClientError),
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
    /// The signed transaction content does not match the original transaction.
    #[error("signed transaction content mismatch: expected hash {expected}, got {received}")]
    ContentMismatch {
        /// The expected signature hash of the original transaction.
        expected: B256,
        /// The signature hash found in the signed envelope.
        received: B256,
    },
}
