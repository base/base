//! Error types for CLI utilities.

use thiserror::Error;

/// Errors that can occur in CLI operations.
#[derive(Error, Debug)]
pub enum CliError {
    /// Error when no unsafe block signer is found for the given chain ID.
    #[error("No unsafe block signer found for chain ID: {0}")]
    UnsafeBlockSignerNotFound(u64),

    /// Error initializing metrics.
    #[error("Failed to initialize metrics")]
    MetricsInitialization(#[from] metrics_exporter_prometheus::BuildError),
}

/// Type alias for CLI results.
pub type CliResult<T> = Result<T, CliError>;
