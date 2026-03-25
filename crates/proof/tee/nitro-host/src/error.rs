//! Error types for host-side TEE prover operations.

use base_proof_tee_nitro_enclave::NitroError;
use thiserror::Error;

/// Top-level error type for host-side nitro prover operations.
#[derive(Debug, Error)]
pub enum NitroHostError {
    /// Enclave error (propagated from the enclave crate).
    #[error(transparent)]
    Enclave(#[from] NitroError),
    /// Transport or protocol error on the vsock channel.
    #[error("transport: {0}")]
    Transport(String),
}
