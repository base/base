//! Error types for op-enclave operations.

use thiserror::Error;

/// Errors that can occur during configuration operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ConfigError {
    /// Binary serialization produced unexpected size.
    #[error("binary serialization failed: expected {expected} bytes, got {actual}")]
    SerializationSize {
        /// Expected number of bytes.
        expected: usize,
        /// Actual number of bytes produced.
        actual: usize,
    },
}

/// Errors that can occur during cryptographic operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum CryptoError {
    /// Signature has invalid length.
    #[error("invalid signature length: expected 65 bytes, got {0}")]
    InvalidSignatureLength(usize),
}

/// Top-level error type for enclave operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum EnclaveError {
    /// Configuration error.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Cryptographic error.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// A specialized Result type for enclave operations.
pub type Result<T> = std::result::Result<T, EnclaveError>;
