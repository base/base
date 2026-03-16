//! Error types for Nitro attestation verification.

use thiserror::Error;

/// Errors that can occur during attestation parsing and verification.
#[derive(Debug, Error)]
pub enum VerifierError {
    /// CBOR encoding or decoding failed.
    #[error("CBOR error: {0}")]
    Cbor(String),

    /// `COSE_Sign1` envelope is malformed.
    #[error("COSE format error: {0}")]
    CoseFormat(String),

    /// Attestation document is malformed or has invalid fields.
    #[error("attestation format error: {0}")]
    AttestationFormat(String),

    /// Certificate chain verification failed.
    #[error("certificate verification error: {0}")]
    CertificateVerification(String),

    /// COSE signature verification failed.
    #[error("signature verification error: {0}")]
    SignatureVerification(String),
}

/// Convenience result alias for verifier operations.
pub type Result<T, E = VerifierError> = std::result::Result<T, E>;
