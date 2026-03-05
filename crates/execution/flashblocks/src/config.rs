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
    /// Whether to enable cached execution when validating blocks against flashblocks.
    pub cached_execution_enabled: bool,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration.
    pub fn new(websocket_url: Url, max_pending_blocks_depth: u64) -> Self {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));
        Self { websocket_url, max_pending_blocks_depth, cached_execution_enabled: false, state }
    }

    /// Set whether cached execution should be enabled in the engine validator.
    pub fn with_cached_execution_enabled(mut self, enabled: bool) -> Self {
        self.cached_execution_enabled = enabled;
        self
    }
}
