use std::sync::Arc;

use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tokio::sync::Semaphore;
use url::Url;

use crate::FlashblocksState;

/// Default max concurrent builder RPC requests.
const DEFAULT_MAX_CONCURRENT_BUILDER_REQUESTS: usize = 10;

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// Maximum number of pending flashblocks to retain in memory.
    pub max_pending_blocks_depth: u64,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
    /// Pre-built HTTP clients for builder RPC endpoints.
    pub builder_clients: Vec<HttpClient>,
    /// Semaphore for rate limiting builder RPC calls.
    pub builder_rpc_semaphore: Arc<Semaphore>,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration.
    pub fn new(
        websocket_url: Url,
        max_pending_blocks_depth: u64,
        builder_rpc: Option<Vec<Url>>,
    ) -> Result<Self, jsonrpsee::core::ClientError> {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));

        let builder_clients = builder_rpc
            .unwrap_or_default()
            .into_iter()
            .map(|url| HttpClientBuilder::default().build(url.as_str()))
            .collect::<Result<Vec<_>, _>>()?;

        let builder_rpc_semaphore =
            Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT_BUILDER_REQUESTS));

        Ok(Self {
            websocket_url,
            max_pending_blocks_depth,
            state,
            builder_clients,
            builder_rpc_semaphore,
        })
    }
}
