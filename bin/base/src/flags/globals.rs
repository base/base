//! Contains the global CLI flags.

use clap::Parser;
use base_cli::LogArgs;

/// Global arguments for the CLI.
#[derive(Parser, Default, Clone, Debug)]
pub struct GlobalArgs {
    /// Logging arguments.
    #[command(flatten)]
    pub log_args: LogArgs,
    /// The network ID (e.g. "8453" or "base-mainnet").
    #[arg(
        long = "network",
        short = 'n',
        global = true,
        default_value = "8453",
        env = "BASE_NETWORK",
        help = "The network ID"
    )]
    pub network: alloy_chains::Chain,
}
