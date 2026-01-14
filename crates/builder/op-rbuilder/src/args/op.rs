//! Additional Node command arguments.
//!
//! Copied from OptimismNode to allow easy extension.

//! clap [Args](clap::Args) for optimism rollup configuration

use std::path::PathBuf;

use alloy_primitives::Address;
use anyhow::{Result, anyhow};
use clap::Parser;
use reth_optimism_cli::commands::Commands;
use reth_optimism_node::args::RollupArgs;

use crate::{gas_limiter::args::GasLimiterArgs, tx_signer::Signer};

/// Parameters for rollup configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct OpRbuilderArgs {
    /// Rollup configuration
    #[command(flatten)]
    pub rollup_args: RollupArgs,
    /// Builder secret key for signing last transaction in block
    #[arg(long = "rollup.builder-secret-key", env = "BUILDER_SECRET_KEY")]
    pub builder_signer: Option<Signer>,

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

    /// Path to builder playgorund to automatically start up the node connected to it
    #[arg(
        long = "builder.playground",
        num_args = 0..=1,
        default_missing_value = "$HOME/.playground/devnet/",
        value_parser = expand_path,
        env = "PLAYGROUND_DIR",
    )]
    pub playground: Option<PathBuf>,
    #[command(flatten)]
    pub flashblocks: FlashblocksArgs,
    #[command(flatten)]
    pub telemetry: TelemetryArgs,
    #[command(flatten)]
    pub gas_limiter: GasLimiterArgs,
}

impl Default for OpRbuilderArgs {
    fn default() -> Self {
        let args = crate::args::Cli::parse_from(["dummy", "node"]);
        let Commands::Node(node_command) = args.command else { unreachable!() };
        node_command.ext
    }
}

fn expand_path(s: &str) -> Result<PathBuf> {
    shellexpand::full(s)
        .map_err(|e| anyhow!("expansion error for `{s}`: {e}"))?
        .into_owned()
        .parse()
        .map_err(|e| anyhow!("invalid path after expansion: {e}"))
}

/// Parameters for Flashblocks configuration
/// The names in the struct are prefixed with `flashblocks` to avoid conflicts
/// with the legacy standard builder configuration (now removed) since these args are
/// flattened into the main `OpRbuilderArgs` struct with the other rollup/node args.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct FlashblocksArgs {
    /// Flashblocks is always enabled; these options tune its behavior.
    /// The port that we bind to for the websocket server that provides flashblocks
    #[arg(long = "flashblocks.port", env = "FLASHBLOCKS_WS_PORT", default_value = "1111")]
    pub flashblocks_port: u16,

    /// The address that we bind to for the websocket server that provides flashblocks
    #[arg(long = "flashblocks.addr", env = "FLASHBLOCKS_WS_ADDR", default_value = "127.0.0.1")]
    pub flashblocks_addr: String,

    /// flashblock block time in milliseconds
    #[arg(long = "flashblocks.block-time", default_value = "250", env = "FLASHBLOCK_BLOCK_TIME")]
    pub flashblocks_block_time: u64,

    /// Builder would always thry to produce fixed number of flashblocks without regard to time of
    /// FCU arrival.
    /// In cases of late FCU it could lead to partially filled blocks.
    #[arg(long = "flashblocks.fixed", default_value = "false", env = "FLASHBLOCK_FIXED")]
    pub flashblocks_fixed: bool,

    /// Time by which blocks would be completed earlier in milliseconds.
    ///
    /// This time used to account for latencies, this time would be deducted from total block
    /// building time before calculating number of fbs.
    #[arg(long = "flashblocks.leeway-time", default_value = "75", env = "FLASHBLOCK_LEEWAY_TIME")]
    pub flashblocks_leeway_time: u64,

    /// Whether to disable state root calculation for each flashblock
    #[arg(
        long = "flashblocks.disable-state-root",
        default_value = "false",
        env = "FLASHBLOCKS_DISABLE_STATE_ROOT"
    )]
    pub flashblocks_disable_state_root: bool,

    /// Flashblocks number contract address
    ///
    /// This is the address of the contract that will be used to increment the flashblock number.
    /// If set a builder tx will be added to the start of every flashblock instead of the regular builder tx.
    #[arg(
        long = "flashblocks.number-contract-address",
        env = "FLASHBLOCK_NUMBER_CONTRACT_ADDRESS"
    )]
    pub flashblocks_number_contract_address: Option<Address>,
}

impl Default for FlashblocksArgs {
    fn default() -> Self {
        let args = crate::args::Cli::parse_from(["dummy", "node"]);
        let Commands::Node(node_command) = args.command else { unreachable!() };
        node_command.ext.flashblocks
    }
}

/// Parameters for telemetry configuration
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct TelemetryArgs {
    /// OpenTelemetry endpoint for traces
    #[arg(long = "telemetry.otlp-endpoint", env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// OpenTelemetry headers for authentication
    #[arg(long = "telemetry.otlp-headers", env = "OTEL_EXPORTER_OTLP_HEADERS")]
    pub otlp_headers: Option<String>,

    /// Inverted sampling frequency in blocks. 1 - each block, 100 - every 100th block.
    #[arg(long = "telemetry.sampling-ratio", env = "SAMPLING_RATIO", default_value = "100")]
    pub sampling_ratio: u64,
}
