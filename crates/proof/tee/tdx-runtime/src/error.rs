use std::{io, path::Path};

use thiserror::Error;

/// Errors produced by TDX runtime signer and Confidential Space token collection code.
#[derive(Debug, Error)]
pub enum TdxRuntimeError {
    /// The signer public key is not an uncompressed 65-byte secp256k1 key.
    #[error("invalid signer public key")]
    InvalidPublicKey,
    /// Failed to sign a proof journal.
    #[error("TDX signer failed to sign data: {0}")]
    Signing(String),
    /// Confidential Space attestation token request failed.
    #[error("Confidential Space attestation token request failed: {0}")]
    AttestationToken(String),
    /// Confidential Space attestation token response was invalid.
    #[error("Confidential Space attestation token response was invalid: {0}")]
    AttestationTokenResponse(String),
    /// Filesystem I/O failed while communicating with the Confidential Space launcher.
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
