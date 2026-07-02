//! Error types for TDX attestation proof generation.

use thiserror::Error;

/// Errors that can occur during TDX attestation proof generation.
#[derive(Debug, Error)]
pub enum ProverError {
    /// The underlying TDX verifier rejected the input.
    #[error("verifier error: {0}")]
    Verifier(#[from] base_proof_tee_tdx_verifier::TdxVerifierError),
    /// Boundless marketplace interaction failed.
    #[error("boundless error: {0}")]
    Boundless(String),
}

/// Convenience result alias for TDX attestation prover operations.
pub type Result<T> = std::result::Result<T, ProverError>;
