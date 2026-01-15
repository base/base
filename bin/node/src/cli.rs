//! Contains the CLI arguments

use base_flashblocks_node::FlashblocksConfig;
use base_txpool::TxpoolConfig;
use reth_optimism_node::args::RollupArgs;

/// CLI Arguments
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup arguments
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// The websocket url used for flashblocks.
    #[arg(long = "websocket-url", value_name = "WEBSOCKET_URL")]
    pub websocket_url: Option<String>,

    /// The max pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing ExEx for mempool-to-block timing analysis
    #[arg(long = "enable-transaction-tracing", value_name = "ENABLE_TRANSACTION_TRACING")]
    pub enable_transaction_tracing: bool,

    /// Enable `info` logs for transaction tracing
    #[arg(
        long = "enable-transaction-tracing-logs",
        value_name = "ENABLE_TRANSACTION_TRACING_LOGS"
    )]
    pub enable_transaction_tracing_logs: bool,

    /// Enable metering RPC for transaction bundle simulation
    #[arg(long = "enable-metering", value_name = "ENABLE_METERING")]
    pub enable_metering: bool,
}

impl Args {
    /// Returns if flashblocks is enabled.
    /// If the websocket url is specified through the CLI.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.websocket_url.is_some()
    }
}

impl From<Args> for Option<FlashblocksConfig> {
    fn from(args: Args) -> Self {
        args.websocket_url.map(|url| FlashblocksConfig::new(url, args.max_pending_blocks_depth))
    }
}

impl From<Args> for TxpoolConfig {
    fn from(args: Args) -> Self {
        Self {
            tracing_enabled: args.enable_transaction_tracing,
            tracing_logs_enabled: args.enable_transaction_tracing_logs,
            sequencer_rpc: args.rollup_args.sequencer,
        }
    }
}
