/// Errors from shared TEE proving operations.
#[derive(Debug, thiserror::Error)]
pub enum TeeProverError {
    /// Transaction serialization failed.
    #[error("serialization failed: {0}")]
    Serialization(String),

    /// Proof encoding failed.
    #[error("encoding failed: {0}")]
    Encoding(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),
}
