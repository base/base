//! L1 head polling source for the batcher service.

use std::sync::Arc;

use alloy_provider::Provider;
use async_trait::async_trait;
use base_batcher_source::{L1HeadPolling, SourceError};

/// Polling source that fetches the latest L1 head block number from an L1 RPC endpoint.
#[derive(derive_more::Debug)]
pub struct RpcL1HeadPollingSource {
    #[debug(skip)]
    provider: Arc<dyn Provider + Send + Sync>,
}

impl RpcL1HeadPollingSource {
    /// Create a new [`RpcL1HeadPollingSource`] wrapping the given provider.
    pub fn new(provider: Arc<dyn Provider + Send + Sync>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl L1HeadPolling for RpcL1HeadPollingSource {
    async fn latest_head(&self) -> Result<u64, SourceError> {
        self.provider.get_block_number().await.map_err(|e| SourceError::Provider(e.to_string()))
    }
}
