use std::sync::Arc;

use url::Url;

use crate::FlashblocksState;

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// Maximum number of pending flashblocks to retain in memory.
    pub max_pending_blocks_depth: u64,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration.
    pub fn new(websocket_url: String, max_pending_blocks_depth: u64) -> Self {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));
        let ws_url = Url::parse(&websocket_url).expect("valid websocket URL");
        Self { websocket_url: ws_url, max_pending_blocks_depth, state }
    }
}
