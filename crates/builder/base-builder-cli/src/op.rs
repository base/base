//! Additional Node command arguments for the OP builder.
//!
//! Copied from OptimismNode to allow easy extension.

use reth_optimism_node::args::RollupArgs;

use crate::{FlashblocksArgs, GasLimiterArgs, TelemetryArgs};

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

    /// How much time extra to wait for the block building job to complete and not get garbage collected
    #[arg(long = "builder.extra-block-deadline-secs", default_value = "20")]
    pub extra_block_deadline_secs: u64,

    /// Whether to enable TIPS Resource Metering
    #[arg(long = "builder.enable-resource-metering", default_value = "false")]
    pub enable_resource_metering: bool,

    /// Buffer size for tx data store (LRU eviction when full)
    #[arg(long = "builder.tx-data-store-buffer-size", default_value = "10000")]
    pub tx_data_store_buffer_size: usize,

    /// Flashblocks configuration
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,
    /// Telemetry configuration
    #[command(flatten)]
    pub telemetry: TelemetryArgs,
    /// Gas limiter configuration
    #[command(flatten)]
    pub gas_limiter: GasLimiterArgs,
}

impl Default for OpRbuilderArgs {
    fn default() -> Self {
        Self {
            rollup_args: RollupArgs::default(),
            chain_block_time: 1000,
            max_gas_per_txn: None,
            extra_block_deadline_secs: 20,
            enable_resource_metering: false,
            tx_data_store_buffer_size: 10000,
            flashblocks: FlashblocksArgs::default(),
            telemetry: TelemetryArgs::default(),
            gas_limiter: GasLimiterArgs::default(),
        }
    }
}
