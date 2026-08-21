//! Core trait for unsafe L2 block sources.

use async_trait::async_trait;
use base_protocol::BlockInfo;

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
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError>;

    /// Reset the source to begin sequential catchup above `safe_head`.
    ///
    /// Called by the driver on resume after a pause, ensuring blocks between
    /// the last safe head and the current unsafe tip are not skipped. The
    /// source should validate the first block against the safe-head hash, then
    /// continue delivering subsequent blocks in order.
    ///
    /// The default implementation is a no-op, suitable for sources that do not
    /// support positional reset (e.g. in-memory test sources).
    fn reset_catchup(&mut self, _safe_head: BlockInfo) {}
}
