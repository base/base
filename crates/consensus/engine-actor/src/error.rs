//! Error types for the engine actor.

use kona_node_service::EngineError;
use thiserror::Error;

/// Errors that can occur in the direct engine processor.
#[derive(Debug, Error)]
pub enum DirectEngineProcessorError {
    /// A channel was closed unexpectedly.
    #[error("channel closed unexpectedly")]
    ChannelClosed,

    /// Engine API error.
    #[error("engine API error: {0}")]
    EngineApi(String),

    /// Configuration error.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// Task error.
    #[error("task error: {0}")]
    Task(String),
}

impl From<EngineError> for DirectEngineProcessorError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::ChannelClosed => Self::ChannelClosed,
            other => Self::Task(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = DirectEngineProcessorError::ChannelClosed;
        assert_eq!(err.to_string(), "channel closed unexpectedly");

        let err = DirectEngineProcessorError::EngineApi("test error".to_string());
        assert_eq!(err.to_string(), "engine API error: test error");
    }
}
