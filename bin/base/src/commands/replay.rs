//! Replay capture utilities.

use base_cli_utils::RuntimeManager;
use base_replay_recorder::RecordArgs;
use clap::{Parser, Subcommand};

/// Replay dataset utilities.
#[derive(Debug, Parser)]
pub(crate) struct ReplayCommand {
    /// Replay operation.
    #[command(subcommand)]
    pub(crate) command: ReplaySubcommand,
}

/// Supported replay operations.
#[derive(Debug, Subcommand)]
pub(crate) enum ReplaySubcommand {
    /// Record HTTP JSON-RPC calls into local or S3 storage.
    Record(RecordArgs),
}

impl ReplaySubcommand {
    /// Dispatch through the cooperative shutdown runtime.
    pub(crate) fn run(self) -> eyre::Result<()> {
        match self {
            Self::Record(args) => RuntimeManager::new()
                .run_until_shutdown(|cancel| async move { Ok(args.run(cancel).await?) }),
        }
    }
}
