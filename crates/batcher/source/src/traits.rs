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

    /// Reset the source to begin sequential catchup from `start_from`.
    ///
    /// Called by the driver on resume after a pause, ensuring blocks between
    /// the last safe head and the current unsafe tip are not skipped. The
    /// source should deliver blocks `start_from, start_from+1, …` sequentially
    /// before switching back to live polling.
    ///
    /// The default implementation is a no-op, suitable for sources that do not
    /// support positional reset (e.g. in-memory test sources).
    fn reset_catchup(&mut self, _start_from: u64) {}
}
