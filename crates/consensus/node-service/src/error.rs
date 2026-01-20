//! Error types for the unified rollup node.

use thiserror::Error;

/// Errors that can occur when running the unified rollup node.
#[derive(Debug, Error)]
pub enum UnifiedRollupNodeError {
    /// The engine actor failed.
    #[error("engine actor failed: {0}")]
    EngineActorFailed(String),

    /// The derivation actor failed.
    #[error("derivation actor failed: {0}")]
    DerivationActorFailed(String),

    /// The node was cancelled.
    #[error("node was cancelled")]
    Cancelled,

    /// Configuration error.
    #[error("configuration error: {0}")]
    Configuration(String),
}
