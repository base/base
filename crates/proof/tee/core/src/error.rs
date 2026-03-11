//! Error types for enclave operations.

use thiserror::Error;

/// Errors that can occur during cryptographic operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum CryptoError {
    /// Signature has invalid length.
    #[error("invalid signature length: expected 65 bytes, got {0}")]
    InvalidSignatureLength(usize),

    /// Invalid ECDSA v-value.
    #[error("invalid ECDSA v-value: expected 0, 1, 27, or 28, got {0}")]
    InvalidVValue(u8),
}

/// Errors that can occur during account proof verification.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProviderError {
    /// Account proof verification failed.
    #[error("account proof verification failed: {0}")]
    AccountProofFailed(String),
}
