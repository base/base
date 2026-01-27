use core::{convert::TryFrom, time::Duration};

use base_builder_cli::{OpRbuilderArgs, ResourceMeteringMode};
use reth_optimism_payload_builder::config::{OpDAConfig, OpGasLimitConfig};

use crate::tx_data_store::TxDataStore;

pub(crate) mod best_txs;
pub(crate) mod config;
pub(crate) mod context;
pub(crate) mod generator;
pub(crate) mod payload;
pub(crate) mod payload_handler;
pub(crate) mod service;
pub(crate) mod wspub;

pub use config::FlashblocksConfig;
pub use context::{FlashblocksExtraCtx, OpPayloadBuilderCtx};
pub use payload::FlashblocksExecutionInfo;
pub use service::FlashblocksServiceBuilder;

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
    /// This is a "use it or lose it" budget per flashblock.
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Block-level state root calculation time budget in microseconds.
    /// Unlike execution time, this is cumulative across the block since state root
    /// is calculated once at the end.
    pub block_state_root_time_budget_us: Option<u128>,

    /// Resource metering mode: off, observe, or enforce.
    pub resource_metering_mode: ResourceMeteringMode,

    /// Unified transaction data store (backrun bundles + resource metering)
    pub tx_data_store: TxDataStore,
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
            .field("resource_metering_mode", &self.resource_metering_mode)
            .field("tx_data_store", &self.tx_data_store)
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
            resource_metering_mode: ResourceMeteringMode::Off,
            tx_data_store: TxDataStore::default(),
        }
    }
}

impl TryFrom<OpRbuilderArgs> for BuilderConfig {
    type Error = eyre::Report;

    fn try_from(args: OpRbuilderArgs) -> Result<Self, Self::Error> {
        let flashblocks = FlashblocksConfig::try_from(args.clone())?;
        Ok(Self {
            block_time: Duration::from_millis(args.chain_block_time),
            block_time_leeway: Duration::from_secs(args.extra_block_deadline_secs),
            da_config: Default::default(),
            gas_limit_config: Default::default(),
            sampling_ratio: args.telemetry.sampling_ratio,
            max_gas_per_txn: args.max_gas_per_txn,
            max_execution_time_per_tx_us: args.max_execution_time_per_tx_us,
            max_state_root_time_per_tx_us: args.max_state_root_time_per_tx_us,
            flashblock_execution_time_budget_us: args.flashblock_execution_time_budget_us,
            block_state_root_time_budget_us: args.block_state_root_time_budget_us,
            resource_metering_mode: args.resource_metering_mode,
            tx_data_store: TxDataStore::new(
                args.resource_metering_mode.is_enabled(),
                args.tx_data_store_buffer_size,
            ),
            flashblocks,
        })
    }
}
