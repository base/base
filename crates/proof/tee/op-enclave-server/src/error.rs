//! Error types for op-enclave-server operations.

use thiserror::Error;

/// Errors that can occur during NSM operations.
#[derive(Debug, Clone, Error)]
pub enum NsmError {
    /// Failed to open NSM session.
    #[error("failed to open NSM session: {0}")]
    SessionOpen(String),
    /// NSM device returned an error.
    #[error("NSM device error: {0}")]
    DeviceError(String),
    /// Failed to describe PCR.
    #[error("failed to describe PCR: {0}")]
    DescribePcr(String),
    /// NSM device did not return PCR data.
    #[error("NSM device did not return PCR data")]
    NoPcrData,
    /// Failed to get attestation.
    #[error("failed to get attestation: {0}")]
    Attestation(String),
    /// NSM device did not return an attestation.
    #[error("NSM device did not return an attestation")]
    NoAttestation,
    /// Failed to get random bytes.
    #[error("failed to get random bytes: {0}")]
    Random(String),
}

/// Errors that can occur during attestation operations.
#[derive(Debug, Clone, Error)]
pub enum AttestationError {
    /// Failed to decode CA roots from base64.
    #[error("failed to decode CA roots: {0}")]
    Base64Decode(String),
    /// CA roots checksum mismatch.
    #[error("CA roots checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// Expected checksum.
        expected: String,
        /// Actual checksum.
        actual: String,
    },
    /// Failed to read CA roots zip.
    #[error("failed to read CA roots zip: {0}")]
    ZipRead(String),
    /// Failed to parse PEM certificate.
    #[error("failed to parse PEM certificate: {0}")]
    PemParse(String),
    /// Failed to verify attestation.
    #[error("failed to verify attestation: {0}")]
    Verification(String),
    /// PCR0 mismatch.
    #[error("attestation PCR0 does not match expected value")]
    Pcr0Mismatch,
    /// Failed to parse CBOR attestation document.
    #[error("failed to parse CBOR attestation: {0}")]
    CborParse(String),
    /// Failed to verify COSE signature.
    #[error("failed to verify COSE signature: {0}")]
    CoseVerify(String),
    /// Certificate chain verification failed.
    #[error("certificate chain verification failed: {0}")]
    CertificateChain(String),
    /// Missing required field in attestation document.
    #[error("missing required field in attestation: {0}")]
    MissingField(String),
    /// Certificate has expired.
    #[error("certificate expired: not valid after {not_after}")]
    CertificateExpired {
        /// The expiry time of the certificate.
        not_after: String,
    },
    /// Certificate is not yet valid.
    #[error("certificate not yet valid: not valid before {not_before}")]
    CertificateNotYetValid {
        /// The not-before time of the certificate.
        not_before: String,
    },
    /// Certificate chain verification failed against trusted root.
    #[error("certificate chain verification failed: {0}")]
    ChainVerificationFailed(String),
    /// X509 store operation error.
    #[error("X509 store error: {0}")]
    X509StoreError(String),
}

/// Errors that can occur during cryptographic operations.
#[derive(Debug, Clone, Error)]
pub enum CryptoError {
    /// Failed to generate RSA key.
    #[error("failed to generate RSA key: {0}")]
    RsaKeyGeneration(String),
    /// Failed to serialize public key to PKIX format.
    #[error("failed to serialize public key to PKIX: {0}")]
    PkixSerialize(String),
    /// Failed to parse PKIX public key.
    #[error("failed to parse PKIX public key: {0}")]
    PkixParse(String),
    /// Failed to encrypt with RSA.
    #[error("RSA encryption failed: {0}")]
    RsaEncrypt(String),
    /// Failed to decrypt with RSA.
    #[error("RSA decryption failed: {0}")]
    RsaDecrypt(String),
    /// Failed to generate ECDSA key.
    #[error("failed to generate ECDSA key: {0}")]
    EcdsaKeyGeneration(String),
    /// Failed to parse ECDSA private key.
    #[error("failed to parse ECDSA private key: {0}")]
    EcdsaKeyParse(String),
    /// Failed to parse hex string.
    #[error("failed to parse hex string: {0}")]
    HexParse(String),
    /// Invalid private key length.
    #[error("invalid private key length: expected 32 bytes, got {0}")]
    InvalidPrivateKeyLength(usize),
}

/// Errors that can occur during proposal operations.
#[derive(Debug, Clone, Error)]
pub enum ProposalError {
    /// No proposals provided for aggregation.
    #[error("no proposals provided for aggregation")]
    EmptyProposals,
    /// Signature verification failed at the given index.
    #[error("invalid signature at proposal index {index}")]
    InvalidSignature {
        /// Index of the proposal with invalid signature.
        index: usize,
    },
    /// Signature is not the expected 65 bytes.
    #[error("invalid signature length: expected 65 bytes, got {0}")]
    InvalidSignatureLength(usize),
    /// ECDSA signing failed.
    #[error("signing failed: {0}")]
    SigningFailed(String),
    /// Public key recovery failed.
    #[error("public key recovery failed: {0}")]
    RecoveryFailed(String),
    /// Intermediate root does not match the proposal's output root at that block.
    #[error("intermediate root mismatch at index {index}: expected {expected}, got {actual}")]
    InvalidIntermediateRoot {
        /// Index of the mismatched intermediate root.
        index: usize,
        /// The output root from the proposal at that block.
        expected: String,
        /// The intermediate root provided by the caller.
        actual: String,
    },
    /// No proposal found for an intermediate root at the given block.
    #[error("no proposal found for intermediate root at block {block}")]
    MissingIntermediateProposal {
        /// The block number where a proposal was expected.
        block: u64,
    },
    /// Core execution failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
}

/// Top-level error type for server operations.
#[derive(Debug, Clone, Error)]
pub enum ServerError {
    /// NSM error.
    #[error(transparent)]
    Nsm(#[from] NsmError),
    /// Attestation error.
    #[error(transparent)]
    Attestation(#[from] AttestationError),
    /// Cryptographic error.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// Proposal error.
    #[error(transparent)]
    Proposal(#[from] ProposalError),
    /// Environment variable error.
    #[error("environment variable error: {0}")]
    EnvVar(String),
}

/// A specialized Result type for server operations.
pub type Result<T> = std::result::Result<T, ServerError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn error_types_are_send_sync() {
        assert_send_sync::<NsmError>();
        assert_send_sync::<AttestationError>();
        assert_send_sync::<CryptoError>();
        assert_send_sync::<ProposalError>();
        assert_send_sync::<ServerError>();
    }

    #[test]
    fn error_types_are_clone() {
        fn assert_clone<T: Clone>() {}

        assert_clone::<NsmError>();
        assert_clone::<AttestationError>();
        assert_clone::<CryptoError>();
        assert_clone::<ProposalError>();
        assert_clone::<ServerError>();
    }
}
