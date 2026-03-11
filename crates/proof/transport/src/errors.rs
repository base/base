use thiserror::Error;

/// Result type for proof transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Errors that can occur during proof transport operations.
#[derive(Error, Debug)]
pub enum TransportError {
    /// An I/O error occurred on the underlying stream.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization or deserialization of a message failed.
    #[error("codec error: {0}")]
    Codec(String),
}
