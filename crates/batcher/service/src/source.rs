//! RPC-based polling source for the latest unsafe L2 head block.

use std::sync::Arc;

use alloy_provider::Provider;
use alloy_rpc_types_eth::BlockNumberOrTag;
use async_trait::async_trait;
use base_alloy_consensus::OpBlock;
use base_alloy_network::Base;
use base_batcher_source::{PollingSource, SourceError};

/// Polling source that fetches the latest unsafe head block from an L2 RPC.
pub struct RpcPollingSource {
    /// The L2 RPC provider.
    provider: Arc<dyn Provider<Base> + Send + Sync>,
}

impl std::fmt::Debug for RpcPollingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcPollingSource").finish_non_exhaustive()
    }
}

impl RpcPollingSource {
    /// Create a new [`RpcPollingSource`] wrapping the given provider.
    pub fn new(provider: Arc<dyn Provider<Base> + Send + Sync>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl PollingSource for RpcPollingSource {
    async fn unsafe_head(&self) -> Result<OpBlock, SourceError> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .full()
            .await
            .map_err(|e| SourceError::Provider(e.to_string()))?
            .ok_or_else(|| SourceError::Provider("latest block not found".to_string()))?
            .into_consensus()
            .map_transactions(|t| t.inner.into_inner());
        Ok(block)
    }
}
