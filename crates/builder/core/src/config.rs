//! Builder Configuration

use core::time::Duration;
use std::sync::Arc;

use base_execution_payload_builder::config::{OpDAConfig, OpGasLimitConfig};

use crate::{
    ExecutionMeteringMode, FlashblocksConfig, NoopMeteringProvider, SharedMeteringProvider,
};

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

    /// Configuration values that are specific to the flashblocks block builder.
    pub flashblocks: FlashblocksConfig,

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

    /// Resource metering provider
    pub metering_provider: SharedMeteringProvider,
}

impl core::fmt::Debug for BuilderConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("block_time", &self.block_time)
            .field("block_time_leeway", &self.block_time_leeway)
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("sampling_ratio", &self.sampling_ratio)
            .field("flashblocks", &self.flashblocks)
            .field("max_gas_per_txn", &self.max_gas_per_txn)
            .field("max_execution_time_per_tx_us", &self.max_execution_time_per_tx_us)
            .field("max_state_root_time_per_tx_us", &self.max_state_root_time_per_tx_us)
            .field("flashblock_execution_time_budget_us", &self.flashblock_execution_time_budget_us)
            .field("block_state_root_time_budget_us", &self.block_state_root_time_budget_us)
            .field("execution_metering_mode", &self.execution_metering_mode)
            .field("max_uncompressed_block_size", &self.max_uncompressed_block_size)
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
            flashblocks: FlashblocksConfig::default(),
            sampling_ratio: 100,
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            max_state_root_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_time_budget_us: None,
            execution_metering_mode: ExecutionMeteringMode::Off,
            max_uncompressed_block_size: None,
            metering_provider: Arc::new(NoopMeteringProvider),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl BuilderConfig {
    /// Creates a new [`BuilderConfig`] suitable for testing with a randomized flashblocks port.
    pub fn for_tests() -> Self {
        use core::net::{Ipv4Addr, SocketAddr};
        let mut config = Self::default();
        // Use port 0 to get a random available port
        config.flashblocks.ws_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
        // Default 1 second block time for tests
        config.block_time = Duration::from_secs(1);
        config
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

    /// Sets the flashblocks configuration.
    #[must_use]
    pub const fn with_flashblocks(mut self, flashblocks: FlashblocksConfig) -> Self {
        self.flashblocks = flashblocks;
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
