//! RPC-based polling source for the latest unsafe L2 head block.

use std::sync::{Arc, Mutex};

use alloy_provider::Provider;
use alloy_rpc_types_eth::BlockNumberOrTag;
use async_trait::async_trait;
use base_alloy_consensus::OpBlock;
use base_alloy_network::Base;
use base_batcher_source::{PollingSource, SourceError};

/// Polling source that fetches the latest unsafe head block from an L2 RPC.
///
/// In normal operation (`new`) the source polls `Latest` on every call.
/// When created with `new_from`, it first catches up sequentially from the
/// given block number before switching to `Latest` polling. This avoids
/// missing blocks between `safe_head + 1` and `latest` on batcher restart.
pub struct RpcPollingSource {
    /// The L2 RPC provider.
    provider: Arc<dyn Provider<Base> + Send + Sync>,
    /// When `Some(n)`, the next `unsafe_head` call fetches block `n`
    /// sequentially (for startup catchup). Cleared when `n` is strictly
    /// greater than the current latest (i.e. block `n` hasn't been produced yet).
    next_sequential: Mutex<Option<u64>>,
}

impl std::fmt::Debug for RpcPollingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcPollingSource").finish_non_exhaustive()
    }
}

impl RpcPollingSource {
    /// Create a new [`RpcPollingSource`] that polls `Latest` on every call.
    pub fn new(provider: Arc<dyn Provider<Base> + Send + Sync>) -> Self {
        Self { provider, next_sequential: Mutex::new(None) }
    }

    /// Create a new [`RpcPollingSource`] that begins sequential catchup from
    /// `start_from`, fetching blocks `start_from, start_from+1, …` in order
    /// before switching to `Latest` polling once it has caught up.
    pub fn new_from(provider: Arc<dyn Provider<Base> + Send + Sync>, start_from: u64) -> Self {
        Self { provider, next_sequential: Mutex::new(Some(start_from)) }
    }
}

#[async_trait]
impl PollingSource for RpcPollingSource {
    async fn unsafe_head(&self) -> Result<OpBlock, SourceError> {
        let sequential = *self.next_sequential.lock().unwrap();

        if let Some(n) = sequential {
            let latest_number = self
                .provider
                .get_block_number()
                .await
                .map_err(|e| SourceError::Provider(e.to_string()))?;

            if n > latest_number {
                // Block n hasn't been produced yet; switch to normal polling.
                *self.next_sequential.lock().unwrap() = None;
            } else {
                let block = self
                    .provider
                    .get_block_by_number(n.into())
                    .full()
                    .await
                    .map_err(|e| SourceError::Provider(e.to_string()))?
                    .ok_or_else(|| SourceError::Provider(format!("block {n} not found")))?
                    .into_consensus()
                    .map_transactions(|t| t.inner.into_inner());
                *self.next_sequential.lock().unwrap() = Some(n + 1);
                return Ok(block);
            }
        }

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
