//! Error types for host-side TEE prover operations.

use base_proof_tee_nitro_enclave::{NitroError, TransportError};
use thiserror::Error;

/// Top-level error type for host-side nitro prover operations.
#[derive(Debug, Error)]
pub enum NitroHostError {
    /// Enclave error (propagated from the enclave crate).
    #[error(transparent)]
    Enclave(#[from] NitroError),
    /// Vsock connection attempt timed out.
    #[error("connect timed out")]
    ConnectTimeout,
    /// Vsock connection attempt failed.
    #[error("connect failed: {0}")]
    ConnectFailed(std::io::Error),
    /// Writing a frame to the transport failed.
    #[error("frame write failed: {0}")]
    FrameWrite(TransportError),
    /// Reading a frame from the transport failed.
    #[error("frame read failed: {0}")]
    FrameRead(TransportError),
    /// The enclave returned a response type that does not match the request.
    #[error("unexpected response for {expected}")]
    UnexpectedResponse {
        /// The request kind that was sent.
        expected: &'static str,
    },
    /// The enclave returned an explicit error response.
    #[error("enclave error: {0}")]
    EnclaveRemoteError(String),
    /// The signer public key returned by the enclave is malformed.
    #[error("invalid signer public key: expected 65-byte uncompressed SEC1 key")]
    InvalidSignerKey,
    /// A response-read timed out (distinct from connect timeout).
    #[error("{operation} timed out")]
    ResponseTimeout {
        /// The operation that timed out.
        operation: &'static str,
    },
}
