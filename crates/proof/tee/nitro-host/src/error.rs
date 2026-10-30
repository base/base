//! Error types for host-side TEE prover operations.

use base_proof_tee_nitro_enclave::{EnclaveLinkError, NitroError};
use thiserror::Error;

/// Top-level error type for host-side nitro prover operations.
#[derive(Debug, Error)]
pub enum NitroHostError {
    /// Enclave error (propagated from the enclave crate).
    #[error(transparent)]
    Enclave(#[from] NitroError),
    /// The Enclave link failed or the enclave rejected the request.
    #[error(transparent)]
    Link(#[from] EnclaveLinkError),
}
