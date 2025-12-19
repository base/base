//! Error types for the encrypted relay.

use thiserror::Error;

use crate::types::RelayErrorCode;

/// Errors that can occur during relay operations.
#[derive(Debug, Error)]
pub enum RelayError {
    /// Cryptographic operation failed.
    #[error("crypto error: {0}")]
    CryptoError(String),

    /// Proof-of-work verification failed.
    #[error("invalid proof-of-work: hash has {found} leading zero bits, required {required}")]
    InvalidPow {
        /// Number of leading zero bits found.
        found: u8,
        /// Number of leading zero bits required.
        required: u8,
    },

    /// Request was rate limited.
    #[error("rate limited")]
    RateLimited,

    /// Unrecognized encryption public key.
    #[error("invalid encryption key: not current or previous")]
    InvalidEncryptionKey,

    /// Payload exceeds maximum size.
    #[error("payload too large: {size} bytes exceeds maximum {max}")]
    PayloadTooLarge {
        /// Actual payload size.
        size: usize,
        /// Maximum allowed size.
        max: usize,
    },

    /// Payload is too small to be valid.
    #[error("payload too small: {size} bytes below minimum {min}")]
    PayloadTooSmall {
        /// Actual payload size.
        size: usize,
        /// Minimum required size.
        min: usize,
    },

    /// Failed to connect to or communicate with sequencer.
    #[error("sequencer unavailable: {0}")]
    SequencerUnavailable(String),

    /// On-chain config read failed.
    #[error("config error: {0}")]
    ConfigError(String),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),

    /// P2P network error.
    #[error("network error: {0}")]
    NetworkError(String),

    /// Attestation verification failed.
    #[error("invalid attestation: {0}")]
    InvalidAttestation(String),

    /// No sequencer peers available for forwarding.
    #[error("no sequencer peers available")]
    NoSequencerPeers,
}

impl RelayError {
    /// Returns the corresponding error code.
    pub const fn code(&self) -> RelayErrorCode {
        match self {
            Self::CryptoError(_) => RelayErrorCode::InternalError,
            Self::InvalidPow { .. } => RelayErrorCode::InvalidPow,
            Self::RateLimited => RelayErrorCode::RateLimited,
            Self::InvalidEncryptionKey => RelayErrorCode::InvalidEncryptionKey,
            Self::PayloadTooLarge { .. } => RelayErrorCode::PayloadTooLarge,
            Self::PayloadTooSmall { .. } => RelayErrorCode::PayloadTooSmall,
            Self::SequencerUnavailable(_) => RelayErrorCode::SequencerUnavailable,
            Self::ConfigError(_) => RelayErrorCode::InternalError,
            Self::Internal(_) => RelayErrorCode::InternalError,
            Self::NetworkError(_) => RelayErrorCode::SequencerUnavailable,
            Self::InvalidAttestation(_) => RelayErrorCode::InternalError,
            Self::NoSequencerPeers => RelayErrorCode::SequencerUnavailable,
        }
    }
}
