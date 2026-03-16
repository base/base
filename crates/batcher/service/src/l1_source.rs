//! L1 head source implementations for the batcher service.

use std::sync::Arc;

use alloy_provider::Provider;
use async_trait::async_trait;
use base_batcher_source::{L1HeadPolling, L1HeadSubscription, SourceError};
use futures::{StreamExt, stream::BoxStream};

/// Polling source that fetches the latest L1 head block number from an L1 RPC endpoint.
pub struct RpcL1HeadPollingSource {
    provider: Arc<dyn Provider + Send + Sync>,
}

impl std::fmt::Debug for RpcL1HeadPollingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcL1HeadPollingSource").finish_non_exhaustive()
    }
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

/// An [`L1HeadSubscription`] backed by a WebSocket provider.
///
/// Owns the WS provider via a type-erased [`Arc`] so the underlying connection
/// is not dropped when the stream is handed to [`HybridL1HeadSource`]. The stream
/// is produced once at construction; [`take_stream`] moves it out on the first call.
///
/// [`HybridL1HeadSource`]: base_batcher_source::HybridL1HeadSource
/// [`take_stream`]: L1HeadSubscription::take_stream
pub struct WsL1HeadSubscription {
    _provider: Arc<dyn std::any::Any + Send + Sync>,
    stream: Option<BoxStream<'static, Result<u64, SourceError>>>,
}

impl WsL1HeadSubscription {
    /// Create a new [`WsL1HeadSubscription`] from a provider and its head number stream.
    pub fn new<P: std::any::Any + Send + Sync + 'static>(
        provider: Arc<P>,
        stream: BoxStream<'static, Result<u64, SourceError>>,
    ) -> Self {
        Self { _provider: provider, stream: Some(stream) }
    }
}

impl std::fmt::Debug for WsL1HeadSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsL1HeadSubscription")
            .field("stream", &self.stream.as_ref().map(|_| "<stream>"))
            .finish_non_exhaustive()
    }
}

impl L1HeadSubscription for WsL1HeadSubscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<u64, SourceError>> {
        self.stream.take().expect("take_stream called more than once")
    }
}

/// A no-op [`L1HeadSubscription`] that never yields head numbers.
///
/// Used when no L1 WebSocket URL is configured; [`HybridL1HeadSource`] falls
/// back entirely to the polling path.
///
/// [`HybridL1HeadSource`]: base_batcher_source::HybridL1HeadSource
#[derive(Debug)]
pub struct NullL1HeadSubscription;

impl L1HeadSubscription for NullL1HeadSubscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<u64, SourceError>> {
        futures::stream::pending().boxed()
    }
}
