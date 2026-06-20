use std::time::Duration;

use alloy_primitives::{Address, B256};
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

    /// Generated proof journal could not be decoded before submission.
    #[error("proof journal could not be decoded: {reason}")]
    InvalidProofJournal {
        /// Decode failure details.
        reason: String,
    },

    /// Generated proof is too old for on-chain registration.
    #[error(
        "attestation proof for signer {signer} is too old: age {age:?} exceeds max {max_age:?}"
    )]
    StaleAttestationProof {
        /// Signer whose registration proof was stale.
        signer: Address,
        /// Proof age at the final pre-submission check.
        age: Duration,
        /// Maximum age configured for registrar-side submission.
        max_age: Duration,
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
