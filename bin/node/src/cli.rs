//! Contains the CLI arguments

use std::net::{IpAddr, SocketAddr};

use base_flashblocks::FlashblocksConfig;
use reth_optimism_node::args::RollupArgs;

/// CLI Arguments
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Rollup")]
pub struct Args {
    /// Rollup arguments
    #[command(flatten)]
    pub rollup_args: RollupArgs,

    /// The max pending blocks depth.
    #[arg(
        long = "max-pending-blocks-depth",
        value_name = "MAX_PENDING_BLOCKS_DEPTH",
        default_value = "3"
    )]
    pub max_pending_blocks_depth: u64,

    /// Enable transaction tracing for mempool-to-block timing analysis
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

    /// Enable the monitoring dashboard web UI
    #[arg(long = "dashboard-enabled")]
    pub dashboard_enabled: bool,

    /// Port for the monitoring dashboard HTTP server
    #[arg(long = "dashboard-port", default_value = "8080")]
    pub dashboard_port: u16,

    /// Address to bind the monitoring dashboard HTTP server
    #[arg(long = "dashboard-addr", default_value = "127.0.0.1")]
    pub dashboard_addr: IpAddr,
}

impl Args {
    /// Returns the dashboard socket address if dashboard is enabled.
    pub fn dashboard_socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.dashboard_addr, self.dashboard_port)
    }
}

impl Args {
    /// Returns if flashblocks is enabled.
    /// If the websocket url is specified through the CLI.
    pub const fn flashblocks_enabled(&self) -> bool {
        self.rollup_args.flashblocks_url.is_some()
    }
}

impl From<&Args> for Option<FlashblocksConfig> {
    fn from(args: &Args) -> Self {
        args.rollup_args
            .flashblocks_url
            .clone()
            .map(|url| FlashblocksConfig::new(url, args.max_pending_blocks_depth))
    }
}
