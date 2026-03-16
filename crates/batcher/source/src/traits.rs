//! Core trait for unsafe L2 block sources.

use async_trait::async_trait;

use crate::{L2BlockEvent, SourceError};

/// A source of unsafe L2 blocks, streaming events as they arrive.
///
/// Implementations must handle both new block delivery and L2 reorg signaling.
/// The batcher driver calls [`next`][UnsafeBlockSource::next] in a loop to drive block ingestion.
#[async_trait]
pub trait UnsafeBlockSource: Send {
    /// Wait for the next L2 block event.
    ///
    /// Blocks (asynchronously) until a new block or reorg is available.
    /// Implementations are responsible for deduplicating blocks seen from multiple
    /// sources (subscription + polling) — if both deliver the same block hash, only
    /// one `Block` event is emitted.
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError>;
}
