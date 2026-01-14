//! JWT error types.

use thiserror::Error;

/// Errors that occur when loading or parsing JWT secrets.
#[derive(Debug, Error)]
pub enum JwtError {
    /// Failed to parse JWT secret from hex.
    #[error("Failed to parse JWT secret: {0}")]
    ParseError(String),
    /// IO error reading/writing JWT file.
    #[error("IO error: {0}")]
    IoError(String),
}

/// Errors that occur during JWT validation with an engine API.
#[derive(Debug, Error)]
pub enum JwtValidationError {
    /// JWT signature is invalid (authentication failed).
    #[error("JWT signature is invalid")]
    InvalidSignature,
    /// Failed to exchange capabilities with engine.
    #[error("Failed to exchange capabilities with engine: {0}")]
    CapabilityExchange(String),
}
