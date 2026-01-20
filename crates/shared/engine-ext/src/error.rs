//! Error types for in-process engine operations.

use thiserror::Error;

/// Errors that can occur when interacting with the engine.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Payload not found in the store.
    #[error("payload not found: {0}")]
    PayloadNotFound(String),

    /// Failed to resolve payload from the store.
    #[error("payload resolution failed: {0}")]
    PayloadResolution(String),

    /// Engine communication failed.
    #[error("engine error: {0}")]
    Engine(String),

    /// Provider query failed.
    #[error("provider error: {0}")]
    Provider(String),

    /// Block not found during lookup.
    #[error("block not found: {0}")]
    BlockNotFound(String),

    /// Forkchoice state has not been initialized.
    #[error("forkchoice state not initialized")]
    ForkchoiceNotInitialized,
}
