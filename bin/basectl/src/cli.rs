//! Contains the CLI arguments for the basectl binary.

use clap::{Parser, Subcommand};
use url::Url;

/// Base infrastructure control CLI.
#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
pub(crate) struct Cli {
    /// Chain configuration (mainnet, sepolia, devnet, or path to config file)
    #[arg(short = 'c', long = "config", default_value = "mainnet", global = true)]
    pub(crate) config: String,
    /// Bootstrap conductor JSON-RPC URL for runtime cluster discovery.
    ///
    /// When set, basectl ignores any hardcoded conductor list in the chain
    /// config and instead asks this URL for the live raft membership, then
    /// polls all discovered peers via templated ports.
    ///
    /// Only applies to the conductor view (and views that embed it, like the
    /// command center). Ignored by `flashblocks --json` and other non-TUI
    /// subcommands.
    #[arg(
        long = "conductor-rpc",
        env = "BASECTL_CONDUCTOR_RPC",
        global = true,
        default_value = "http://localhost:5545"
    )]
    pub(crate) conductor_rpc: Option<Url>,
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

/// Subcommands for the basectl CLI.
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Chain configuration operations
    #[command(visible_alias = "c")]
    Config,
    /// Flashblocks operations
    #[command(visible_alias = "f")]
    Flashblocks {
        /// Output flashblocks as JSON lines instead of the TUI
        #[arg(long)]
        json: bool,
    },
    /// DA (Data Availability) backlog monitor
    #[command(visible_alias = "d")]
    Da,
    /// Command center (combined view)
    #[command(visible_alias = "cc")]
    CommandCenter,
    /// HA conductor cluster monitor
    #[command(visible_alias = "co")]
    Conductor,
    /// Network upgrade activation countdown and history
    #[command(visible_alias = "u")]
    Upgrades,
}
