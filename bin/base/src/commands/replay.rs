//! Replay recording and playback utilities.

use base_cli_utils::RuntimeManager;
use base_replay_driver::RunCommand;
use base_replay_export::ExportCommand;
use base_replay_recorder::RecordArgs;
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
    /// Record HTTP JSON-RPC calls into local or S3 storage.
    Record(RecordArgs),
    /// Export recorded ingress for a canonical L2 block range.
    Export(ExportCommand),
    /// Replay exported ingress against an existing builder and emit a diff.
    Run(RunCommand),
}

impl ReplaySubcommand {
    /// Execute the selected replay operation.
    pub fn run(self) -> eyre::Result<()> {
        match self {
            Self::Record(args) => RuntimeManager::new()
                .run_until_shutdown(|cancel| async move { Ok(args.run(cancel).await?) }),
            Self::Export(command) => {
                CliRunner::try_default_runtime()?.run_blocking_until_ctrl_c(async move {
                    command.execute().await?;
                    Ok(())
                })
            }
            Self::Run(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let executor = runner.runtime();
                runner.run_blocking_until_ctrl_c(async move { command.execute(&executor).await })
            }
        }
    }
}
