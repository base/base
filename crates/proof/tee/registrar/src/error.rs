use base_proof_tee_nitro_attestation_prover::ProverError;
use base_tx_manager::TxManagerError;
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

    /// Public key returned by a prover instance is malformed.
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),

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

    /// Transaction submission or confirmation failed (RPC, nonce, fee, timeout).
    #[error("transaction error")]
    Transaction(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Configuration is invalid.
    #[error("config error: {0}")]
    Config(String),
}

impl From<ProverError> for RegistrarError {
    fn from(e: ProverError) -> Self {
        Self::ProofGeneration(Box::new(e))
    }
}

impl From<TxManagerError> for RegistrarError {
    fn from(e: TxManagerError) -> Self {
        Self::Transaction(Box::new(e))
    }
}

/// Convenience result alias for registrar operations.
pub type Result<T, E = RegistrarError> = std::result::Result<T, E>;
