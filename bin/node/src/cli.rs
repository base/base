//! Contains the CLI arguments

use std::sync::Arc;

use base_reth_runner::{BaseNodeConfig, FlashblocksConfig, RunnerFlashblocksCell, TracingConfig};
use once_cell::sync::OnceCell;
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

impl From<Args> for BaseNodeConfig {
    fn from(args: Args) -> Self {
        let flashblocks_cell: RunnerFlashblocksCell = Arc::new(OnceCell::new());
        let flashblocks = args.websocket_url.map(|websocket_url| FlashblocksConfig {
            websocket_url,
            max_pending_blocks_depth: args.max_pending_blocks_depth,
        });

        Self {
            rollup_args: args.rollup_args,
            flashblocks,
            tracing: TracingConfig {
                enabled: args.enable_transaction_tracing,
                logs_enabled: args.enable_transaction_tracing_logs,
            },
            metering_enabled: args.enable_metering,
            flashblocks_cell,
        }
    }
}
