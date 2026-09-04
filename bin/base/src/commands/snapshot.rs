//! `base snapshot` subcommand group: snapshot manifest generation and download.

use base_execution_cli::{
    chainspec::BaseChainSpecParser,
    commands::{SnapshotManifestCommand, download},
};
use base_node_core::BaseNode;
use clap::{Parser, Subcommand};
use reth_cli_runner::CliRunner;

/// Snapshot manifest generation and download utilities.
#[derive(Parser, Debug)]
pub(crate) struct SnapshotCommand {
    /// The snapshot subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: SnapshotSubcommand,
}

impl SnapshotCommand {
    pub(crate) fn run(self) -> eyre::Result<()> {
        self.command.run()
    }
}

/// Subcommands for `base snapshot`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub(crate) enum SnapshotSubcommand {
    /// Generate modular chunk archives and a snapshot manifest.
    #[command(name = "manifest")]
    Manifest(SnapshotManifestCommand),
    /// Download Base node snapshots from R2 storage.
    #[command(name = "download")]
    Download(Box<download::BaseDownloadCommand<BaseChainSpecParser>>),
}

impl SnapshotSubcommand {
    pub(crate) fn run(self) -> eyre::Result<()> {
        match self {
            Self::Manifest(command) => {
                command.execute()?;
                Ok(())
            }
            Self::Download(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_blocking_until_ctrl_c((*command).execute::<BaseNode>())
            }
        }
    }
}
