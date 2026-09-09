//! L1 head source implementations for the batcher service.

use std::sync::Arc;

use alloy_provider::Provider;
use async_trait::async_trait;
use base_batcher_source::{KeepAliveSubscription, L1HeadPolling, PendingSubscription, SourceError};

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

/// A WebSocket-backed L1 head subscription.
///
/// Owns the WS provider so the underlying connection is not dropped when the
/// stream is handed to [`HybridL1HeadSource`].
///
/// [`HybridL1HeadSource`]: base_batcher_source::HybridL1HeadSource
pub type WsL1HeadSubscription = KeepAliveSubscription<u64>;

/// A no-op L1 head subscription that never yields head numbers.
///
/// Used when no L1 WebSocket URL is configured; [`HybridL1HeadSource`] falls
/// back entirely to the polling path.
///
/// [`HybridL1HeadSource`]: base_batcher_source::HybridL1HeadSource
pub type NullL1HeadSubscription = PendingSubscription<u64>;
