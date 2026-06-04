//! Checkpoint actor error types.

/// Error returned by checkpoint actor operations.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    /// Database error.
    #[error("checkpoint database error: {0}")]
    Database(String),
    /// Checkpoint actor channel closed.
    #[error("checkpoint actor channel closed")]
    ChannelClosed,
    /// Checkpoint actor response was dropped.
    #[error("checkpoint actor response dropped")]
    ResponseDropped,
}
