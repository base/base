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
    /// Whether to enable cached execution via the flashblocks-aware engine validator.
    pub cached_execution: bool,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
    /// Whether to compute per-transaction state roots during flashblock processing.
    pub simulate_state_root: bool,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration.
    pub fn new(websocket_url: Url, max_pending_blocks_depth: u64) -> Self {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));
        Self {
            websocket_url,
            max_pending_blocks_depth,
            cached_execution: false,
            state,
            simulate_state_root: false,
        }
    }

    /// Enables per-transaction state root simulation during flashblock processing.
    pub const fn with_simulate_state_root(mut self, enabled: bool) -> Self {
        self.simulate_state_root = enabled;
        self
    }
}
