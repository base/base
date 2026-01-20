//! Error types for the engine actor.

use thiserror::Error;

/// Errors that can occur in the engine actor.
#[derive(Debug, Error)]
pub enum EngineActorError {
    /// The request channel was closed.
    #[error("request channel closed")]
    ChannelClosed,

    /// The actor was cancelled.
    #[error("actor cancelled")]
    Cancelled,

    /// Processor error.
    #[error("processor error: {0}")]
    Processor(#[from] ProcessorError),
}

/// Errors that can occur during request processing.
#[derive(Debug, Error)]
pub enum ProcessorError {
    /// Build request failed.
    #[error("build failed: {0}")]
    Build(String),

    /// Seal request failed.
    #[error("seal failed: {0}")]
    Seal(String),

    /// Consolidation request failed.
    #[error("consolidation failed: {0}")]
    Consolidation(String),

    /// Finalization request failed.
    #[error("finalization failed: {0}")]
    Finalization(String),

    /// Unsafe block processing failed.
    #[error("unsafe block processing failed: {0}")]
    UnsafeBlock(String),

    /// Reset request failed.
    #[error("reset failed: {0}")]
    Reset(String),

    /// Engine error.
    #[error("engine error: {0}")]
    Engine(String),

    /// Missing payload ID after FCU.
    #[error("missing payload ID after fork choice update")]
    MissingPayloadId,

    /// Invalid payload status.
    #[error("invalid payload status: {0}")]
    InvalidPayloadStatus(String),

    /// Block not found.
    #[error("block not found: {0}")]
    BlockNotFound(String),
}

impl ProcessorError {
    /// Creates an engine error from any error type.
    pub fn engine<E: std::error::Error>(e: E) -> Self {
        Self::Engine(e.to_string())
    }
}
