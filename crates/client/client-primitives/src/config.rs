//! Configuration types for extensions.

/// Flashblocks-specific configuration knobs.
#[derive(Debug, Clone)]
pub struct FlashblocksConfig {
    /// The websocket endpoint that streams flashblock updates.
    pub websocket_url: String,
    /// Maximum number of pending flashblocks to retain in memory.
    pub max_pending_blocks_depth: u64,
}

/// Transaction tracing toggles.
#[derive(Debug, Clone, Copy)]
pub struct TracingConfig {
    /// Enables the transaction tracing ExEx.
    pub enabled: bool,
    /// Emits `info`-level logs for the tracing ExEx when enabled.
    pub logs_enabled: bool,
}
