//! Block subscription implementations for the batcher service.

use std::sync::Arc;

use base_alloy_consensus::OpBlock;
use base_batcher_source::{BlockSubscription, SourceError};
use futures::{StreamExt, stream::BoxStream};

/// A [`BlockSubscription`] backed by a WebSocket provider.
///
/// Owns the WS provider via a type-erased [`Arc`] so the underlying connection
/// is not dropped when the stream is handed to [`HybridBlockSource`]. The stream
/// is produced once at construction; [`take_stream`] moves it out on the first call.
///
/// The provider is stored as `Arc<dyn Any + Send + Sync>` because the exact
/// alloy provider type varies by transport and we only need to hold a reference
/// for keepalive purposes, not to call any methods on it.
///
/// [`HybridBlockSource`]: base_batcher_source::HybridBlockSource
/// [`take_stream`]: BlockSubscription::take_stream
pub struct WsBlockSubscription {
    _provider: Arc<dyn std::any::Any + Send + Sync>,
    stream: Option<BoxStream<'static, Result<OpBlock, SourceError>>>,
}

impl WsBlockSubscription {
    /// Create a new [`WsBlockSubscription`] from a provider and its subscription stream.
    ///
    /// `provider` can be any `Send + Sync + 'static` type — typically the alloy
    /// WS root provider returned by [`ProviderBuilder::connect`].
    pub fn new<P: std::any::Any + Send + Sync + 'static>(
        provider: Arc<P>,
        stream: BoxStream<'static, Result<OpBlock, SourceError>>,
    ) -> Self {
        Self { _provider: provider, stream: Some(stream) }
    }
}

impl std::fmt::Debug for WsBlockSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsBlockSubscription")
            .field("stream", &self.stream.as_ref().map(|_| "<stream>"))
            .finish_non_exhaustive()
    }
}

impl BlockSubscription for WsBlockSubscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<OpBlock, SourceError>> {
        self.stream.take().expect("take_stream called more than once")
    }
}

/// A no-op [`BlockSubscription`] that never yields blocks.
///
/// Used when the L2 RPC is not a WebSocket URL and subscription is unavailable;
/// [`HybridBlockSource`] will rely entirely on the polling path.
///
/// [`HybridBlockSource`]: base_batcher_source::HybridBlockSource
#[derive(Debug)]
pub struct NullSubscription;

impl BlockSubscription for NullSubscription {
    fn take_stream(&mut self) -> BoxStream<'static, Result<OpBlock, SourceError>> {
        futures::stream::pending().boxed()
    }
}
