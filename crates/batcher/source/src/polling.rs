//! Polling source trait for fetching the current unsafe head block.

use async_trait::async_trait;
use base_alloy_consensus::OpBlock;

use crate::SourceError;

/// A provider that can return the current unsafe head block by polling.
#[async_trait]
pub trait PollingSource: Send + Sync {
    /// Fetch the current unsafe head block.
    async fn unsafe_head(&self) -> Result<OpBlock, SourceError>;

    /// Reset the source to begin sequential catchup from `start_from`.
    ///
    /// After this call the next [`unsafe_head`][Self::unsafe_head] invocation
    /// should return block `start_from`, then `start_from + 1`, and so on,
    /// until the source has caught up to the chain tip.
    ///
    /// The default implementation is a no-op, suitable for sources that
    /// do not support positional reset (e.g. test stubs).
    fn reset_catchup(&self, _start_from: u64) {}
}
