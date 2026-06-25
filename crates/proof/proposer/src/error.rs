//! Error types for the proposer.

use base_proof_rpc::RpcError;
use base_proof_submission::ProofSubmissionError;
use thiserror::Error;

/// Main error type for the proposer.
#[derive(Debug, Error)]
pub enum ProposerError {
    /// RPC communication error.
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcError),

    /// Prover server error.
    #[error("prover error: {0}")]
    Prover(String),

    /// Contract interaction error.
    #[error("contract error: {0}")]
    Contract(String),

    /// Proof submission error.
    #[error(transparent)]
    Submission(#[from] ProofSubmissionError),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),

    /// Internal logic error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ProposerError {
    /// Returns the metrics label for this error variant.
    pub const fn metric_label(&self) -> &'static str {
        match self {
            Self::Rpc(_) => "rpc",
            Self::Prover(_) => "prover",
            Self::Contract(_) => "contract",
            Self::Submission(err) => err.metric_label(),
            Self::Config(_) => "config",
            Self::Internal(_) => "internal",
        }
    }
}
