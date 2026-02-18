/// Configuration types for the payload builder.
use core::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use std::sync::Arc;

use reth_optimism_payload_builder::config::{OpDAConfig, OpGasLimitConfig};

use crate::{ExecutionMeteringMode, NoopMeteringProvider, SharedMeteringProvider};

/// Configuration values specific to the flashblocks builder.
///
/// Controls flashblock timing, WebSocket publishing, and state root
/// computation settings for progressive block construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashblocksConfig {
    /// The address of the websockets endpoint that listens for subscriptions to
    /// new flashblocks updates.
    pub ws_addr: SocketAddr,

    /// How often a flashblock is produced. This is independent of the block time of the chain.
    /// Each block will contain one or more flashblocks. On average, the number of flashblocks
    /// per block is equal to the block time divided by the flashblock interval.
    pub interval: Duration,

    /// How much time would be deducted from block build time to account for latencies in
    /// milliseconds.
    ///
    /// If `dynamic_adjustment` is false this value would be deducted from first flashblock and
    /// it shouldn't be more than interval
    ///
    /// If `dynamic_adjustment` is true this value would be deducted from first flashblock and
    /// it shouldn't be more than interval
    pub leeway_time: Duration,

    /// Disables dynamic flashblocks number adjustment based on FCU arrival time
    pub fixed: bool,

    /// Should we disable state root calculation for each flashblock
    pub disable_state_root: bool,

    /// Whether to compute state root only when `get_payload` is called (finalization).
    /// When enabled, flashblocks are built without state root, but the final payload
    /// returned by `get_payload` will have the state root computed.
    pub compute_state_root_on_finalize: bool,
}

impl Default for FlashblocksConfig {
    fn default() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 1111),
            interval: Duration::from_millis(250),
            leeway_time: Duration::from_millis(50),
            fixed: false,
            disable_state_root: false,
            compute_state_root_on_finalize: false,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl FlashblocksConfig {
    /// Creates a new [`FlashblocksConfig`] suitable for testing with a randomized port.
    pub fn for_tests() -> Self {
        Self {
            ws_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
            interval: Duration::from_millis(200),
            leeway_time: Duration::from_millis(100),
            fixed: false,
            disable_state_root: false,
            compute_state_root_on_finalize: false,
        }
    }

    /// Sets the flashblock interval in milliseconds.
    #[must_use]
    pub const fn with_interval_ms(mut self, ms: u64) -> Self {
        self.interval = Duration::from_millis(ms);
        self
    }

    /// Sets the leeway time in milliseconds.
    #[must_use]
    pub const fn with_leeway_time_ms(mut self, ms: u64) -> Self {
        self.leeway_time = Duration::from_millis(ms);
        self
    }

    /// Sets whether flashblocks are fixed.
    #[must_use]
    pub const fn with_fixed(mut self, fixed: bool) -> Self {
        self.fixed = fixed;
        self
    }

    /// Sets whether state root calculation is disabled.
    #[must_use]
    pub const fn with_disable_state_root(mut self, disable: bool) -> Self {
        self.disable_state_root = disable;
        self
    }

    /// Sets whether state root is computed on finalize.
    #[must_use]
    pub const fn with_compute_state_root_on_finalize(mut self, compute: bool) -> Self {
        self.compute_state_root_on_finalize = compute;
        self
    }

    /// Sets the WebSocket port.
    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.ws_addr.set_port(port);
        self
    }
}

/// Configuration for the payload builder.
///
/// Contains the subset of builder configuration needed for payload construction.
/// The full `BuilderConfig` in the core crate constructs this via `payload_config()`.
#[derive(Clone)]
pub struct PayloadBuilderConfig {
    /// Block time interval.
    pub block_time: Duration,
    /// Flashblocks timing config.
    pub flashblocks: FlashblocksConfig,
    /// DA constraints.
    pub da_config: OpDAConfig,
    /// Gas limit settings.
    pub gas_limit_config: OpGasLimitConfig,
    /// Per-tx gas cap.
    pub max_gas_per_txn: Option<u64>,
    /// Per-tx execution time cap (us).
    pub max_execution_time_per_tx_us: Option<u128>,
    /// Per-tx state root time cap (us).
    pub max_state_root_time_per_tx_us: Option<u128>,
    /// Flashblock execution time budget (us).
    pub flashblock_execution_time_budget_us: Option<u128>,
    /// Block state root time budget (us).
    pub block_state_root_time_budget_us: Option<u128>,
    /// Metering mode.
    pub execution_metering_mode: ExecutionMeteringMode,
    /// Sampling ratio for log verbosity.
    pub sampling_ratio: u64,
    /// Resource metering provider.
    pub metering_provider: SharedMeteringProvider,
}

impl PayloadBuilderConfig {
    /// Returns the number of flashblocks per block.
    pub const fn flashblocks_per_block(&self) -> u64 {
        if self.block_time.as_millis() == 0 {
            return 0;
        }
        (self.block_time.as_millis() / self.flashblocks.interval.as_millis()) as u64
    }
}

impl core::fmt::Debug for PayloadBuilderConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PayloadBuilderConfig")
            .field("block_time", &self.block_time)
            .field("flashblocks", &self.flashblocks)
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("max_gas_per_txn", &self.max_gas_per_txn)
            .field("max_execution_time_per_tx_us", &self.max_execution_time_per_tx_us)
            .field("max_state_root_time_per_tx_us", &self.max_state_root_time_per_tx_us)
            .field("flashblock_execution_time_budget_us", &self.flashblock_execution_time_budget_us)
            .field("block_state_root_time_budget_us", &self.block_state_root_time_budget_us)
            .field("execution_metering_mode", &self.execution_metering_mode)
            .field("sampling_ratio", &self.sampling_ratio)
            .field("metering_provider", &self.metering_provider)
            .finish()
    }
}

impl Default for PayloadBuilderConfig {
    fn default() -> Self {
        Self {
            block_time: Duration::from_secs(2),
            flashblocks: FlashblocksConfig::default(),
            da_config: OpDAConfig::default(),
            gas_limit_config: OpGasLimitConfig::default(),
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            max_state_root_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_time_budget_us: None,
            execution_metering_mode: ExecutionMeteringMode::Off,
            sampling_ratio: 100,
            metering_provider: Arc::new(NoopMeteringProvider),
        }
    }
}
