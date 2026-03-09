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

    /// Maximum state root calculation time per transaction in microseconds.
    pub max_state_root_time_per_tx_us: Option<u128>,

    /// Flashblock-level execution time budget in microseconds.
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Block-level state root calculation time budget in microseconds.
    pub block_state_root_time_budget_us: Option<u128>,

    /// Execution metering mode: off, dry-run, or enforce.
    pub execution_metering_mode: ExecutionMeteringMode,

    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes.
    pub max_uncompressed_block_size: Option<u64>,

    /// When true, use a fixed number of flashblocks per block (block_time / interval)
    /// instead of dynamically adjusting based on time drift.
    pub fixed: bool,

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
            .field("max_state_root_time_per_tx_us", &self.max_state_root_time_per_tx_us)
            .field("flashblock_execution_time_budget_us", &self.flashblock_execution_time_budget_us)
            .field("block_state_root_time_budget_us", &self.block_state_root_time_budget_us)
            .field("execution_metering_mode", &self.execution_metering_mode)
            .field("max_uncompressed_block_size", &self.max_uncompressed_block_size)
            .field("fixed", &self.fixed)
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
            max_state_root_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_time_budget_us: None,
            execution_metering_mode: ExecutionMeteringMode::Off,
            max_uncompressed_block_size: None,
            fixed: false,
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

    /// Sets the fixed flashblocks mode.
    #[must_use]
    pub const fn with_fixed(mut self, fixed: bool) -> Self {
        self.fixed = fixed;
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
}
