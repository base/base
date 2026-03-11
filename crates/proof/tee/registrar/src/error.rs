use thiserror::Error;

/// Errors that can occur in the prover registrar.
#[derive(Debug, Error)]
pub enum RegistrarError {
    /// Instance discovery failed.
    #[error("instance discovery failed: {0}")]
    Discovery(String),

    /// Failed to contact a prover instance.
    #[error("prover client error for instance {instance}: {reason}")]
    ProverClient {
        /// The instance ID or IP that was being contacted.
        instance: String,
        /// The underlying error.
        reason: String,
    },

    /// ZK proof generation failed.
    #[error("proof generation failed: {0}")]
    ProofGeneration(String),

    /// On-chain registry operation failed.
    #[error("registry error: {0}")]
    Registry(String),

    /// Transaction signing or submission failed.
    #[error("signing error: {0}")]
    Signing(String),

    /// Configuration is invalid.
    #[error("config error: {0}")]
    Config(String),
}

/// Convenience result alias for registrar operations.
pub type Result<T, E = RegistrarError> = std::result::Result<T, E>;
