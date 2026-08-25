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
    /// The requested block has not been produced yet. Raised during sequential catchup when
    /// the chain head is still below the first batchable block (e.g. only the genesis block
    /// exists): the source signals the caller to wait rather than returning an earlier,
    /// un-batchable block.
    #[error("l2 block {requested} not yet produced (chain head at {latest})")]
    NotReady {
        /// The block number the source is waiting for.
        requested: u64,
        /// The current chain head block number.
        latest: u64,
    },
    /// The underlying channel or stream was closed.
    #[error("source closed")]
    Closed,
}
