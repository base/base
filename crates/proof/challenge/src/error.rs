//! Error types for the challenger.

use thiserror::Error;

/// Main error type for the challenger.
#[derive(Debug, Error)]
pub enum ChallengerError {
    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),

    /// RPC connection error.
    #[error("RPC error: {0}")]
    Rpc(String),

    /// Shutdown error.
    #[error("shutdown error: {0}")]
    Shutdown(String),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<eyre::Error> for ChallengerError {
    fn from(err: eyre::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

/// Result type alias for challenger operations.
pub type ChallengerResult<T> = Result<T, ChallengerError>;
