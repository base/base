//! CLI definitions for managing local L1+L2 devnet.

use clap::{Parser, Subcommand};

/// Manage local L1+L2 devnet
#[derive(Parser, Debug)]
#[command(name = "devnet", about = "Manage local L1+L2 devnet")]
pub struct DevnetCli {
    /// The command to execute
    #[command(subcommand)]
    pub command: Command,
}

/// Available subcommands for the devnet CLI
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the devnet
    Start,
    /// Show devnet status
    Status,
    /// Display available accounts
    Accounts,
    /// Run smoke tests
    Smoke,
    /// Clean up devnet
    Clean,
    /// Flashblocks operations
    Flashblocks,
}
