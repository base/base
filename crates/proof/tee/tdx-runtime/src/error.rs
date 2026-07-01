use std::{io, path::Path, time::SystemTimeError};

use thiserror::Error;

/// Errors produced by TDX runtime signer and quote collection code.
#[derive(Debug, Error)]
pub enum TdxRuntimeError {
    /// The signer public key is not an uncompressed 65-byte secp256k1 key.
    #[error("invalid signer public key")]
    InvalidPublicKey,
    /// Failed to sign a proof journal.
    #[error("TDX signer failed to sign data: {0}")]
    Signing(String),
    /// The TSM/configfs provider reported a non-TDX backend.
    #[error("TSM/configfs provider is not a TDX guest provider: {0}")]
    UnexpectedConfigfsProvider(String),
    /// The TSM/configfs report generation changed unexpectedly.
    #[error(
        "TSM/configfs report generation changed while collecting a quote: expected {expected}, got {actual}"
    )]
    ConfigfsGenerationMismatch {
        /// Expected generation counter after this quote request.
        expected: u64,
        /// Actual generation counter read from configfs.
        actual: u64,
    },
    /// Quote generation failed.
    #[error("TDX quote generation failed: {0}")]
    QuoteGeneration(String),
    /// System clock is before the Unix epoch.
    #[error("system clock error: {0}")]
    SystemTime(#[from] SystemTimeError),
    /// Filesystem I/O failed while collecting a quote.
    #[error("filesystem error at {path}: {source}")]
    Filesystem {
        /// Path that failed.
        path: String,
        /// Underlying I/O error.
        source: io::Error,
    },
}

impl TdxRuntimeError {
    /// Creates a filesystem error from a `Path`.
    pub fn filesystem_at(path: &Path, source: io::Error) -> Self {
        Self::Filesystem { path: path.display().to_string(), source }
    }
}

/// Result alias for TDX runtime operations.
pub type Result<T> = std::result::Result<T, TdxRuntimeError>;
