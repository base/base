//! `base snapshot` subcommand group: snapshot manifest generation and download.

use base_execution_cli::{
    chainspec::BaseChainSpecParser,
    commands::{MaintenanceCommand, SnapshotManifestCommand, download},
};
use clap::{Parser, Subcommand};

/// Snapshot manifest generation and download utilities.
#[derive(Parser, Debug)]
pub(crate) struct SnapshotCommand {
    /// The snapshot subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: SnapshotSubcommand,
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
            Self::Manifest(command) => command.execute(),
            Self::Download(command) => {
                MaintenanceCommand::Download(*command).run_with_default_runner()
            }
        }
    }
}
