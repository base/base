//! Polling source trait for fetching the current unsafe head block.

use async_trait::async_trait;
use base_alloy_consensus::OpBlock;

use crate::SourceError;

/// A provider that can return the current unsafe head block by polling.
#[async_trait]
pub trait PollingSource: Send + Sync {
    /// Fetch the current unsafe head block.
    async fn unsafe_head(&self) -> Result<OpBlock, SourceError>;
}
