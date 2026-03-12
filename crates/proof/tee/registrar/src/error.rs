use thiserror::Error;

/// Errors that can occur in the prover registrar.
#[derive(Debug, Error)]
pub enum RegistrarError {
    /// Instance discovery failed.
    #[error("instance discovery failed")]
    Discovery(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Failed to contact a prover instance.
    #[error("prover client error for instance {instance}")]
    ProverClient {
        /// The instance ID or IP that was being contacted.
        instance: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// ZK proof generation failed.
    #[error("proof generation failed")]
    ProofGeneration(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// On-chain registry operation failed.
    #[error("registry error")]
    Registry(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An on-chain registry contract call failed.
    #[error("registry call failed: {context}")]
    RegistryCall {
        /// Description of the call that failed (e.g. `"isValidSigner(0x1234…)"`).
        context: String,
        /// The underlying contract call error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Transaction signing or submission failed.
    #[error("signing error")]
    Signing(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Configuration is invalid.
    #[error("config error: {0}")]
    Config(String),
}

/// Convenience result alias for registrar operations.
pub type Result<T, E = RegistrarError> = std::result::Result<T, E>;
