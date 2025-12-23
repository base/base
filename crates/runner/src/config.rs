//! Contains the Base node configuration structures.

use reth_optimism_node::args::RollupArgs;

use crate::extensions::FlashblocksCell;

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
    /// Configuration for priority fee estimation.
    pub metering: MeteringConfig,
    /// Shared Flashblocks state cache.
    pub flashblocks_cell: FlashblocksCell,
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

/// Configuration for priority fee estimation.
#[derive(Debug, Clone)]
pub struct MeteringConfig {
    /// Whether metering is enabled.
    pub enabled: bool,
    /// Kafka configuration for bundle events.
    pub kafka: Option<KafkaConfig>,
    /// Resource limits for fee estimation.
    pub resource_limits: ResourceLimitsConfig,
    /// Percentile for recommended priority fee (0.0-1.0).
    pub priority_fee_percentile: f64,
    /// Default priority fee when resource is not congested (in wei).
    pub uncongested_priority_fee: u128,
    /// Number of recent blocks to retain in metering cache.
    pub cache_size: usize,
}

/// Kafka connection configuration.
#[derive(Debug, Clone)]
pub struct KafkaConfig {
    /// Comma-separated broker addresses.
    pub brokers: String,
    /// Topic name for accepted bundle events.
    pub topic: String,
    /// Consumer group ID.
    pub group_id: String,
    /// Optional path to properties file.
    pub properties_file: Option<String>,
}

/// Resource limits for priority fee estimation.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimitsConfig {
    /// Gas limit per flashblock.
    pub gas_limit: u64,
    /// Execution time budget in microseconds.
    pub execution_time_us: u64,
    /// State root time budget in microseconds (optional).
    pub state_root_time_us: Option<u64>,
    /// Data availability bytes limit.
    pub da_bytes: u64,
}
