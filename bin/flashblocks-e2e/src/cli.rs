//! CLI argument parsing and configuration.

use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// End-to-end regression testing for node-reth flashblocks RPC.
#[derive(Parser, Debug)]
#[command(name = "flashblocks-e2e")]
#[command(about = "End-to-end regression testing for node-reth flashblocks RPC")]
pub(crate) struct Args {
    /// HTTP RPC endpoint URL for the node being tested.
    #[arg(long, default_value = "http://localhost:8545")]
    pub rpc_url: String,

    /// Flashblocks WebSocket URL (sequencer/builder endpoint).
    #[arg(long, default_value = "wss://mainnet.flashblocks.base.org/ws")]
    pub flashblocks_ws_url: String,

    /// Recipient address for ETH transfers in tests.
    /// Required when PRIVATE_KEY is set.
    #[arg(long)]
    pub recipient: Option<String>,

    /// Simulator contract address for state root timing tests.
    /// If not provided, state root timing tests will use simple ETH transfers.
    #[arg(long, env = "SIMULATOR_ADDRESS")]
    pub simulator: Option<String>,

    /// Run only tests matching this filter (supports glob patterns).
    #[arg(long, short)]
    pub filter: Option<String>,

    /// List available tests without running them.
    #[arg(long)]
    pub list: bool,

    /// Continue running tests even after failures.
    #[arg(long, default_value = "false")]
    pub keep_going: bool,

    /// Verbose output (can be repeated for more verbosity).
    #[arg(long, short, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Output format: text, json.
    #[arg(long, default_value = "text")]
    pub format: OutputFormat,
}

/// Output format for test results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable text output with colors.
    Text,
    /// JSON output for CI integration.
    Json,
}

/// Initialize tracing with the specified verbosity level.
pub(crate) fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "flashblocks_e2e=info,base_testing_flashblocks_e2e=info",
        1 => "flashblocks_e2e=debug,base_testing_flashblocks_e2e=debug",
        _ => "flashblocks_e2e=trace,base_testing_flashblocks_e2e=trace",
    };

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)))
        .init();
}
