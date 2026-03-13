//! Proofs database inspection commands.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod get;
mod stats;

/// Proofs database inspection tool.
#[derive(Debug, Parser)]
pub struct DbCommand {
    #[arg(long = "proofs-history.storage-path", value_name = "PROOFS_HISTORY_STORAGE_PATH")]
    storage_path: PathBuf,

    #[command(subcommand)]
    command: DbSubcommands,
}

impl DbCommand {
    /// Execute the `proofs db` command.
    pub fn execute(self) -> eyre::Result<()> {
        let storage = base_execution_trie::MdbxProofsStorage::new(&self.storage_path)?;

        match self.command {
            DbSubcommands::Stats(cmd) => cmd.execute(&storage),
            DbSubcommands::Get(cmd) => cmd.execute(&storage),
        }
    }
}

#[derive(Debug, Subcommand)]
enum DbSubcommands {
    /// Show table sizes and entry counts
    #[command(name = "stats")]
    Stats(stats::StatsCommand),
    /// Look up a value by key
    #[command(name = "get")]
    Get(get::GetCommand),
}
