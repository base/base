//! Replay recording and playback utilities.

use base_replay_export::ExportCommand;
use clap::{Parser, Subcommand};
use reth_cli_runner::CliRunner;

/// Replay dataset utilities.
#[derive(Debug, Parser)]
pub struct ReplayCommand {
    /// Replay operation.
    #[command(subcommand)]
    pub command: ReplaySubcommand,
}

/// Operations on replay datasets.
#[derive(Debug, Subcommand)]
pub enum ReplaySubcommand {
    /// Export recorded ingress for a canonical L2 block range.
    Export(ExportCommand),
}

impl ReplaySubcommand {
    /// Execute the selected replay operation.
    pub fn run(self) -> eyre::Result<()> {
        let runner = CliRunner::try_default_runtime()?;
        match self {
            Self::Export(command) => runner.run_blocking_until_ctrl_c(async move {
                command.execute().await?;
                Ok(())
            }),
        }
    }
}
