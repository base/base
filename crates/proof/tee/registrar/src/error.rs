use std::time::Duration;

use alloy_primitives::{Address, B256};
use base_proof_contracts::ContractError;
use base_proof_tee_nitro_verifier::VerifierError;
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

    /// A certificate cache transaction was mined but did not produce usable cache state.
    #[error("certificate cache transaction {tx_hash} for {cert_hash} reverted")]
    CertificateCacheReverted {
        /// Certificate cache key.
        cert_hash: B256,
        /// Hash of the reverted transaction.
        tx_hash: B256,
    },

    /// Attestation failed registrar-side validation.
    #[error("invalid attestation: {0}")]
    InvalidAttestationProof(String),

    /// Attestation parsing or hint generation failed.
    #[error(transparent)]
    Planning(#[from] PlannerError),

    /// Attestation is too old for on-chain registration.
    #[error("attestation for signer {signer} is too old: age {age:?} exceeds max {max_age:?}")]
    StaleAttestationProof {
        /// Signer whose attestation was stale.
        signer: Address,
        /// Attestation age at the final pre-submission check.
        age: Duration,
        /// Maximum age configured for registrar-side submission.
        max_age: Duration,
    },

    /// Attestation timestamp is not strictly before the current Unix second.
    #[error("attestation for signer {signer} is from the future: {timestamp_ms} ms")]
    FutureAttestationProof {
        /// Signer whose attestation timestamp is invalid.
        signer: Address,
        /// Attestation timestamp in Unix milliseconds.
        timestamp_ms: u64,
    },

    /// A certificate required by the attestation is expired.
    #[error("certificate {label} expired at Unix timestamp {not_after}")]
    ExpiredCertificate {
        /// Human-readable certificate role.
        label: String,
        /// X.509 expiration timestamp in Unix seconds.
        not_after: u64,
    },

    /// A certificate required by the attestation is revoked.
    #[error("certificate {label} is revoked: {cert_id}")]
    RevokedCertificate {
        /// Human-readable certificate role.
        label: String,
        /// Issuer/serial revocation identity.
        cert_id: B256,
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

/// Errors that can occur while parsing a Nitro attestation into a registration plan.
#[derive(Debug, Error)]
pub enum PlannerError {
    /// Strict COSE / payload CBOR validation failed (`NitroValidator` parity).
    #[error("COSE format error: {0}")]
    Cose(String),

    /// Underlying attestation decode / content validation failure from `nitro-verifier`.
    #[error("attestation parse error")]
    Parse(#[from] VerifierError),

    /// Attestation document is missing fields required for Base registration.
    #[error("attestation format error: {0}")]
    Attestation(String),

    /// Certificate parsing or `CertManager` key derivation failed.
    #[error("certificate error")]
    Certificate(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Attestation `public_key` cannot be converted to a signer address.
    #[error("public key error: {0}")]
    PublicKey(String),

    /// P-384 inverse-hint generation failed.
    #[error(transparent)]
    Hint(#[from] HintError),
}

/// Errors from the Agora / `nitro-validator` P-384 inverse-hint transcript.
#[derive(Debug, Error)]
pub enum HintError {
    /// Signature, key, or arithmetic input rejected by the verifier transcript.
    #[error("{0}")]
    Rejected(String),

    /// Certificate DER could not be parsed into P-384 verify inputs.
    #[error("certificate error: {0}")]
    Certificate(String),
}

/// Convenience result alias for hint generation.
pub type HintResult<T> = std::result::Result<T, HintError>;

/// Convenience result alias for planner operations.
pub type PlannerResult<T> = std::result::Result<T, PlannerError>;
