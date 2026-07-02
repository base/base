//! Error types for TDX attestation proof generation.

use thiserror::Error;

/// Errors that can occur during TDX attestation proof generation.
#[derive(Debug, Error)]
pub enum ProverError {
    /// The encoded TDX prover input is malformed.
    #[error("input decode error: {0}")]
    InputDecode(String),
    /// The underlying TDX verifier rejected the input.
    #[error("verifier error: {0}")]
    Verifier(#[from] base_proof_tee_tdx_verifier::TdxVerifierError),
    /// The decoded input signer does not match the signer being registered.
    #[error("signer mismatch: expected {expected}, got {actual}")]
    SignerMismatch {
        /// Signer supplied by the registrar.
        expected: alloy_primitives::Address,
        /// Signer committed by the TDX verifier input.
        actual: alloy_primitives::Address,
    },
    /// Boundless marketplace interaction failed.
    #[error("boundless error: {0}")]
    Boundless(String),
}

/// Convenience result alias for TDX attestation prover operations.
pub type Result<T> = std::result::Result<T, ProverError>;
