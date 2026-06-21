use alloy_primitives::B256;
use base_proof_contracts::ContractError;
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
    ProofGeneration(#[from] ProverError),

    /// Shared contract client call failed.
    #[error(transparent)]
    Contract(#[from] ContractError),

    /// Transaction submission or confirmation failed (RPC, nonce, fee, timeout).
    #[error("transaction error")]
    Transaction(#[from] TxManagerError),

    /// Registration transaction was mined but reverted.
    #[error("registration transaction {tx_hash} reverted")]
    ReceiptReverted {
        /// Hash of the reverted transaction.
        tx_hash: B256,
    },

    /// Configuration is invalid.
    #[error("config error: {0}")]
    Config(String),

    /// Service lifecycle setup failed.
    #[error("service error: {0}")]
    Service(String),

    /// CRL (Certificate Revocation List) check failed.
    #[error("CRL error: {0}")]
    Crl(#[from] crate::crl::CrlError),
}

/// Convenience result alias for registrar operations.
pub type Result<T> = std::result::Result<T, RegistrarError>;
