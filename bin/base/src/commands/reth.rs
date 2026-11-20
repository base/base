//! `base reth` subcommand group: execution-layer maintenance utilities.

use base_execution_cli::commands::RethMaintenanceSubcommand;
use clap::Parser;

/// Execution-layer maintenance utilities re-exposed from `base-reth-node`.
#[derive(Parser, Debug)]
pub(crate) struct RethCommand {
    /// The reth-derived subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: RethMaintenanceSubcommand,
}
