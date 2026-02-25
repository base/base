use std::{fmt, sync::Arc};

use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use url::Url;

use crate::{AdaptiveConcurrencyLimiter, BuilderRpcStats, FlashblocksState};

/// Default max concurrent builder RPC requests.
const DEFAULT_MAX_CONCURRENT_BUILDER_REQUESTS: usize = 10;

/// Flashblocks-specific configuration knobs.
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// Maximum number of pending flashblocks to retain in memory.
    pub max_pending_blocks_depth: u64,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
    /// Builder RPC clients.
    pub builder_clients: Vec<(HttpClient, Arc<BuilderRpcStats>)>,
    /// Adaptive concurrency limiter.
    pub concurrency_limiter: Arc<AdaptiveConcurrencyLimiter>,
}

impl Clone for FlashblocksConfig {
    fn clone(&self) -> Self {
        Self {
            websocket_url: self.websocket_url.clone(),
            max_pending_blocks_depth: self.max_pending_blocks_depth,
            state: Arc::clone(&self.state),
            builder_clients: self.builder_clients.clone(),
            concurrency_limiter: Arc::clone(&self.concurrency_limiter),
        }
    }
}

impl fmt::Debug for FlashblocksConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksConfig")
            .field("websocket_url", &self.websocket_url)
            .field("max_pending_blocks_depth", &self.max_pending_blocks_depth)
            .field("builder_clients_count", &self.builder_clients.len())
            .field("concurrency_limiter", &self.concurrency_limiter)
            .finish()
    }
}
impl FlashblocksConfig {
    /// Creates a new Flashblocks configuration.
    pub fn new(
        websocket_url: Url,
        max_pending_blocks_depth: u64,
        builder_rpc: Option<Vec<Url>>,
        max_concurrent_builder_requests: Option<usize>,
    ) -> Result<Self, jsonrpsee::core::ClientError> {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));

        let builder_clients = builder_rpc
            .unwrap_or_default()
            .into_iter()
            .map(|url| -> Result<_, jsonrpsee::core::ClientError> {
                let client = HttpClientBuilder::default().build(url.as_str())?;
                let stats = Arc::new(BuilderRpcStats::new(url.as_str()));
                Ok((client, stats))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let max_concurrent =
            max_concurrent_builder_requests.unwrap_or(DEFAULT_MAX_CONCURRENT_BUILDER_REQUESTS);
        let concurrency_limiter = Arc::new(AdaptiveConcurrencyLimiter::new(max_concurrent));

        Ok(Self {
            websocket_url,
            max_pending_blocks_depth,
            state,
            builder_clients,
            concurrency_limiter,
        })
    }
}
