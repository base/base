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

    /// x509 certificate parsing failed.
    #[error("x509 parse error: {0}")]
    X509Parse(String),

    /// Certificate chain verification failed (M-01 checks).
    #[error("certificate verification error: {0}")]
    CertificateVerification(String),

    /// COSE signature verification failed.
    #[error("signature verification error: {0}")]
    SignatureVerification(String),

    /// Attestation content validation failed (M-02 checks).
    #[error("content validation error: {0}")]
    ContentValidation(String),
}

/// Convenience result alias for verifier operations.
pub type Result<T, E = VerifierError> = std::result::Result<T, E>;
