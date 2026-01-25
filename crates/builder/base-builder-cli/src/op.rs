//! Additional Node command arguments for the OP builder.
//!
//! Copied from OptimismNode to allow easy extension.

use clap::ValueEnum;
use reth_optimism_node::args::RollupArgs;

use crate::{FlashblocksArgs, TelemetryArgs};

/// Resource metering mode for transaction time limits.
///
/// Controls how the builder handles time-based resource limits
/// (execution time and state root calculation time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ResourceMeteringMode {
    /// Resource metering is disabled. No time limit checks are performed.
    #[default]
    Off,
    /// Dry-run mode: collect metrics about transactions that would exceed time limits,
    /// but don't actually reject them. Useful for gathering data before enforcement.
    DryRun,
    /// Enforce mode: reject transactions that exceed time limits.
    Enforce,
}

impl ResourceMeteringMode {
    /// Returns true if metering data should be collected (dry-run or enforce mode).
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::DryRun | Self::Enforce)
    }

    /// Returns true if limits should only be observed in dry-run mode, not enforced.
    pub const fn is_dry_run(&self) -> bool {
        matches!(self, Self::DryRun)
    }
}

/// Parameters for rollup configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct OpRbuilderArgs {
    /// Rollup configuration
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// chain block time in milliseconds
    #[arg(long = "rollup.chain-block-time", default_value = "1000", env = "CHAIN_BLOCK_TIME")]
    pub chain_block_time: u64,

    /// max gas a transaction can use
    #[arg(long = "builder.max_gas_per_txn")]
    pub max_gas_per_txn: Option<u64>,

    /// Maximum execution time per transaction in microseconds (requires resource metering)
    #[arg(long = "builder.max-execution-time-per-tx-us")]
    pub max_execution_time_per_tx_us: Option<u128>,

    /// Maximum state root calculation time per transaction in microseconds (requires resource metering)
    #[arg(long = "builder.max-state-root-time-per-tx-us")]
    pub max_state_root_time_per_tx_us: Option<u128>,

    /// Flashblock-level execution time budget in microseconds (use it or lose it per flashblock)
    #[arg(long = "builder.flashblock-execution-time-budget-us")]
    pub flashblock_execution_time_budget_us: Option<u128>,

    /// Block-level state root calculation time budget in microseconds (cumulative across block)
    #[arg(long = "builder.block-state-root-time-budget-us")]
    pub block_state_root_time_budget_us: Option<u128>,

    /// How much extra time to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,

    /// Resource metering mode: off (disabled), dry-run (collect metrics without enforcing),
    /// or enforce (reject transactions exceeding time limits)
    #[arg(long = "builder.resource-metering-mode", default_value = "off", value_enum)]
    pub resource_metering_mode: ResourceMeteringMode,

    /// Buffer size for tx data store (LRU eviction when full)
    #[arg(long = "builder.tx-data-store-buffer-size", default_value = "10000")]
    pub tx_data_store_buffer_size: usize,

    /// Flashblocks configuration
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,
    /// Telemetry configuration
    #[command(flatten)]
    pub telemetry: TelemetryArgs,
}

impl Default for OpRbuilderArgs {
    fn default() -> Self {
        Self {
            rollup_args: RollupArgs::default(),
            chain_block_time: 1000,
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            max_state_root_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_time_budget_us: None,
            extra_block_deadline_secs: 20,
            resource_metering_mode: ResourceMeteringMode::Off,
            tx_data_store_buffer_size: 10000,
            flashblocks: FlashblocksArgs::default(),
            telemetry: TelemetryArgs::default(),
        }
    }
}
