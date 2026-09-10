//! Polling source trait for fetching the current L1 head block number.

use async_trait::async_trait;

use crate::SourceError;

/// A provider that can return the current L1 head block number by polling.
#[async_trait]
pub trait L1HeadPolling: Send + Sync {
    /// Fetch the current L1 head block number.
    async fn latest_head(&self) -> Result<u64, SourceError>;
}
