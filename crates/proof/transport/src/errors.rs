use thiserror::Error;

/// Errors that can occur during proof transport operations.
#[derive(Error, Debug)]
pub enum TransportError {
    /// An I/O error occurred on the underlying stream.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The blocking task panicked or was cancelled.
    #[cfg(feature = "vsock")]
    #[error("task failed: {0}")]
    TaskFailed(#[from] tokio::task::JoinError),

    /// Serialization or deserialization of a message failed.
    #[error("codec error: {0}")]
    Codec(String),

    /// The proof execution itself failed.
    #[error("prove execution failed: {0}")]
    ProveExecution(String),
}
