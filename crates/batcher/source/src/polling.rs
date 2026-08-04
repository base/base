//! Polling source trait for fetching L2 blocks by number.

use async_trait::async_trait;
use base_common_consensus::BaseBlock;

use crate::SourceError;

/// A provider that can fetch an L2 block by number.
#[async_trait]
pub trait PollingSource: Send + Sync {
    /// Fetch `number`, returning [`SourceError::BlockUnavailable`] when the
    /// provider does not have it yet.
    async fn block_by_number(&self, number: u64) -> Result<BaseBlock, SourceError>;
}
