use std::{sync::Arc, time::Duration};

use url::Url;

use crate::FlashblocksState;

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: Url,
    /// Maximum number of pending blocks retained ahead of the canonical tip.
    ///
    /// Counts canonical block numbers, not flashblock indices. When pending is
    /// not based on the provider tip, or `latest - canonical` exceeds this
    /// depth, the processor clears the snapshot until an index-0 flashblock
    /// rooted at that tip arrives. The node CLI default is `3`.
    pub max_pending_blocks_depth: u64,
    /// Interval between upstream websocket ping frames.
    pub subscriber_ping_interval: Duration,
    /// Shared Flashblocks state.
    pub state: Arc<FlashblocksState>,
}

impl FlashblocksConfig {
    /// Create a new Flashblocks configuration.
    pub fn new(websocket_url: Url, max_pending_blocks_depth: u64) -> Self {
        let state = Arc::new(FlashblocksState::new(max_pending_blocks_depth));
        Self {
            websocket_url,
            max_pending_blocks_depth,
            subscriber_ping_interval: Duration::from_secs(30),
            state,
        }
    }

    /// Set the interval between upstream websocket ping frames.
    pub const fn with_subscriber_ping_interval(
        mut self,
        subscriber_ping_interval: Duration,
    ) -> Self {
        assert!(!subscriber_ping_interval.is_zero(), "ping interval must be positive");
        self.subscriber_ping_interval = subscriber_ping_interval;
        self
    }
}
