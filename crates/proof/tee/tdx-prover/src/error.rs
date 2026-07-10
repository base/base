use base_proof_preimage::PreimageKey;
use base_proof_tee_tdx_runtime::TdxRuntimeError;
use base_proof_tee_tdx_verifier::TdxVerifierError;
use thiserror::Error;

/// Errors that can occur while serving TDX proofs.
#[derive(Debug, Error)]
pub enum TdxProverError {
    /// TDX runtime quote or signer operation failed.
    #[error(transparent)]
    Runtime(#[from] TdxRuntimeError),
    /// TDX quote parsing or measurement extraction failed.
    #[error(transparent)]
    Verifier(#[from] TdxVerifierError),
    /// The proof execution pipeline failed.
    #[error("proof pipeline error: {0}")]
    ProofPipeline(String),
    /// The requested chain ID is not supported by the TEE prover config cache.
    #[error("unsupported chain ID: {0}")]
    UnsupportedChain(u64),
    /// A preimage's content does not match its hash-based key.
    #[error("preimage hash mismatch for key {0}")]
    InvalidPreimage(PreimageKey),
}

/// A specialized result type for TDX prover operations.
pub type Result<T> = std::result::Result<T, TdxProverError>;
