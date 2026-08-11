//! Error types for block sources.

/// Errors produced by block sources.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The source has no more blocks (used by [`InMemoryBlockSource`][crate::test_utils::InMemoryBlockSource] when empty).
    #[error("block source exhausted")]
    Exhausted,
    /// Provider or RPC error.
    #[error("provider error: {0}")]
    Provider(String),
    /// A requested block has not become available from the provider yet.
    #[error("block {0} is not available yet")]
    BlockUnavailable(u64),
    /// The underlying channel or stream was closed.
    #[error("source closed")]
    Closed,
}
