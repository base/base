//! Contains the Base node configuration structures.

use reth_optimism_node::args::RollupArgs;

/// Captures the pieces of CLI configuration that the node logic cares about.
#[derive(Debug, Clone)]
pub struct BaseNodeConfig {
    /// Rollup-specific arguments forwarded to the Optimism node implementation.
    pub rollup_args: RollupArgs,
    /// Optional flashblocks configuration if the websocket URL was provided.
    pub flashblocks: Option<FlashblocksConfig>,
    /// Execution extension tracing toggles.
    pub tracing: TracingConfig,
    /// Indicates whether the metering RPC surface should be installed.
    pub metering_enabled: bool,
}

impl BaseNodeConfig {
    /// Returns `true` if flashblocks support should be wired up.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.flashblocks.is_some()
    }
}

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
