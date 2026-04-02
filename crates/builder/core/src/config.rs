//! Builder Configuration

use core::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use std::sync::Arc;

use base_execution_payload_builder::config::{OpDAConfig, OpGasLimitConfig};

use crate::{ExecutionMeteringMode, NoopMeteringProvider, SharedMeteringProvider};

/// Configuration values for the flashblocks builder.
#[derive(Clone)]
pub struct BuilderConfig {
    /// The interval at which blocks are added to the chain.
    /// This is also the frequency at which the builder will be receiving FCU requests from the
    /// sequencer.
    pub block_time: Duration,

    /// Data Availability configuration for the OP builder
    /// Defines constraints for the maximum size of data availability transactions.
    pub da_config: OpDAConfig,

    /// Gas limit configuration for the payload builder
    pub gas_limit_config: OpGasLimitConfig,

    /// Extra time allowed for payload building before garbage collection.
    pub block_time_leeway: Duration,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    pub sampling_ratio: u64,

    /// The address of the websockets endpoint that listens for subscriptions to
    /// new flashblocks updates.
    pub flashblocks_ws_addr: SocketAddr,

    /// How often a flashblock is produced. This is independent of the block time of the chain.
    pub flashblocks_interval: Duration,

    /// How much time would be deducted from block build time to account for latencies.
    /// This value would be deducted from first flashblock and it shouldn't be more than interval.
    pub flashblocks_leeway_time: Duration,

    /// Maximum gas a transaction can use before being excluded.
    pub max_gas_per_txn: Option<u64>,

    /// Maximum execution time per transaction in microseconds.
    pub max_execution_time_per_tx_us: Option<u128>,

    /// Flashblock-level execution time budget in microseconds.
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Block-level state root gas limit.
    ///
    /// State root gas is a synthetic resource that accumulates like gas but penalizes
    /// transactions whose simulated state root cost is disproportionate to their gas usage.
    /// For each metered transaction: `sr_gas = gas_used × (1 + K × max(0, SR_ms - anchor))`.
    /// Normal transactions (SR ≤ anchor) pay 1:1. State-heavy transactions pay more.
    pub block_state_root_gas_limit: Option<u64>,

    /// State root gas coefficient (K). Controls how aggressively excess SR time
    /// inflates the state root gas cost. Default: 0.02.
    pub state_root_gas_coefficient: f64,

    /// State root gas anchor in microseconds. SR time below this threshold
    /// produces no penalty (multiplier = 1). Default: 5000 (5ms).
    pub state_root_gas_anchor_us: u128,

    /// Execution metering mode: off, dry-run, or enforce.
    pub execution_metering_mode: ExecutionMeteringMode,

    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes.
    pub max_uncompressed_block_size: Option<u64>,

    /// Duration to wait for metering data before including a transaction.
    /// Transactions younger than this without metering data will be skipped.
    pub metering_wait_duration: Option<Duration>,

    /// Resource metering provider
    pub metering_provider: SharedMeteringProvider,
}

impl BuilderConfig {
    /// Returns the number of flashblocks per block.
    pub const fn flashblocks_per_block(&self) -> u64 {
        if self.block_time.as_millis() == 0 {
            return 0;
        }
        (self.block_time.as_millis() / self.flashblocks_interval.as_millis()) as u64
    }
}

impl core::fmt::Debug for BuilderConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("block_time", &self.block_time)
            .field("block_time_leeway", &self.block_time_leeway)
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("sampling_ratio", &self.sampling_ratio)
            .field("flashblocks_ws_addr", &self.flashblocks_ws_addr)
            .field("flashblocks_interval", &self.flashblocks_interval)
            .field("flashblocks_leeway_time", &self.flashblocks_leeway_time)
            .field("max_gas_per_txn", &self.max_gas_per_txn)
            .field("max_execution_time_per_tx_us", &self.max_execution_time_per_tx_us)
            .field("flashblock_execution_time_budget_us", &self.flashblock_execution_time_budget_us)
            .field("block_state_root_gas_limit", &self.block_state_root_gas_limit)
            .field("state_root_gas_coefficient", &self.state_root_gas_coefficient)
            .field("state_root_gas_anchor_us", &self.state_root_gas_anchor_us)
            .field("execution_metering_mode", &self.execution_metering_mode)
            .field("max_uncompressed_block_size", &self.max_uncompressed_block_size)
            .field("metering_wait_duration", &self.metering_wait_duration)
            .field("metering_provider", &self.metering_provider)
            .finish()
    }
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            block_time: Duration::from_secs(2),
            block_time_leeway: Duration::from_millis(500),
            da_config: OpDAConfig::default(),
            gas_limit_config: OpGasLimitConfig::default(),
            flashblocks_ws_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 1111),
            flashblocks_interval: Duration::from_millis(250),
            flashblocks_leeway_time: Duration::from_millis(50),
            sampling_ratio: 100,
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_gas_limit: None,
            state_root_gas_coefficient: 0.02,
            state_root_gas_anchor_us: 5_000,
            execution_metering_mode: ExecutionMeteringMode::Off,
            max_uncompressed_block_size: None,
            metering_wait_duration: None,
            metering_provider: Arc::new(NoopMeteringProvider),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl BuilderConfig {
    /// Creates a new [`BuilderConfig`] suitable for testing with a randomized flashblocks port.
    pub fn for_tests() -> Self {
        Self {
            flashblocks_ws_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
            flashblocks_interval: Duration::from_millis(200),
            flashblocks_leeway_time: Duration::from_millis(100),
            block_time: Duration::from_secs(1),
            ..Self::default()
        }
    }

    /// Sets the block time in milliseconds.
    #[must_use]
    pub const fn with_block_time_ms(mut self, ms: u64) -> Self {
        self.block_time = Duration::from_millis(ms);
        self
    }

    /// Sets the maximum gas per transaction.
    #[must_use]
    pub const fn with_max_gas_per_txn(mut self, max_gas: Option<u64>) -> Self {
        self.max_gas_per_txn = max_gas;
        self
    }

    /// Sets the flashblocks leeway time in milliseconds.
    #[must_use]
    pub const fn with_flashblocks_leeway_time_ms(mut self, ms: u64) -> Self {
        self.flashblocks_leeway_time = Duration::from_millis(ms);
        self
    }

    /// Sets the flashblocks interval in milliseconds.
    #[must_use]
    pub const fn with_flashblocks_interval_ms(mut self, ms: u64) -> Self {
        self.flashblocks_interval = Duration::from_millis(ms);
        self
    }

    /// Sets the maximum uncompressed block size.
    #[must_use]
    pub const fn with_max_uncompressed_block_size(
        mut self,
        max_uncompressed_block_size: Option<u64>,
    ) -> Self {
        self.max_uncompressed_block_size = max_uncompressed_block_size;
        self
    }

    /// Sets the metering wait duration.
    #[must_use]
    pub const fn with_metering_wait_duration(
        mut self,
        metering_wait_duration: Option<Duration>,
    ) -> Self {
        self.metering_wait_duration = metering_wait_duration;
        self
    }
}
