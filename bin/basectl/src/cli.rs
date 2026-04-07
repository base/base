//! Contains the CLI arguments for the basectl binary.

use clap::{Parser, Subcommand};

/// Base infrastructure control CLI.
#[derive(Debug, Parser)]
#[command(name = "basectl")]
#[command(about = "Base infrastructure control CLI")]
pub(crate) struct Cli {
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
        /// Network to connect to (mainnet, sepolia, devnet, or a config file path).
        /// Only used with --json; the TUI uses the in-app network picker.
        #[arg(long, default_value = "mainnet")]
        network: String,
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
}
