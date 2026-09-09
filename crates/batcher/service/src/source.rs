//! RPC-based source for fetching L2 blocks by number.

use std::sync::Arc;

use alloy_provider::Provider;
use async_trait::async_trait;
use base_batcher_source::{PollingSource, SourceError};
use base_common_consensus::BaseBlock;
use base_common_network::Base;

/// Fetches full L2 blocks from an RPC provider.
#[derive(derive_more::Debug)]
pub struct RpcPollingSource {
    /// The L2 RPC provider.
    #[debug(skip)]
    provider: Arc<dyn Provider<Base> + Send + Sync>,
}

impl RpcPollingSource {
    /// Create a new [`RpcPollingSource`].
    pub fn new(provider: Arc<dyn Provider<Base> + Send + Sync>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl PollingSource for RpcPollingSource {
    async fn block_by_number(&self, number: u64) -> Result<BaseBlock, SourceError> {
        let block = self
            .provider
            .get_block_by_number(number.into())
            .full()
            .await
            .map_err(|e| SourceError::Provider(e.to_string()))?
            .ok_or(SourceError::BlockUnavailable(number))?
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|t| t.inner.into_inner());
        Ok(block)
    }
}
