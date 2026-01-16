//! Error types for the direct engine driver.

use thiserror::Error;

/// Errors that can occur when interacting with the engine.
#[derive(Debug, Error)]
pub enum DirectEngineError {
    /// The engine returned an invalid payload status.
    #[error("invalid payload status: {0}")]
    InvalidPayloadStatus(String),

    /// The payload was not found.
    #[error("payload not found: {0}")]
    PayloadNotFound(String),

    /// The block was not found.
    #[error("block not found: {0}")]
    BlockNotFound(String),

    /// Transport error (HTTP, IPC, etc.).
    #[error("transport error: {0}")]
    Transport(String),

    /// The engine channel was closed.
    #[error("engine channel closed")]
    ChannelClosed,

    /// Internal engine error.
    #[error("internal engine error: {0}")]
    Internal(String),
}
