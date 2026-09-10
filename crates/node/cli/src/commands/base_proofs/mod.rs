//! Base Proofs management commands

use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use clap::{Parser, Subcommand};

pub mod init;
pub mod prune;
pub mod unwind;

/// `base-node base-proofs` command
#[derive(Debug, Parser)]
pub struct Command {
    #[command(subcommand)]
    command: Subcommands,
}

impl Command {
    /// Execute `base-proofs` command
    pub async fn execute(self, runtime: base_common_runtime::Runtime) -> eyre::Result<()> {
        match self.command {
            Subcommands::Init(cmd) => cmd.execute(runtime.clone()).await,
            Subcommands::Prune(cmd) => cmd.execute(runtime.clone()).await,
            Subcommands::Unwind(cmd) => cmd.execute(runtime).await,
        }
    }
}

impl Command {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        match &self.command {
            Subcommands::Init(cmd) => cmd.chain_spec(),
            Subcommands::Prune(cmd) => cmd.chain_spec(),
            Subcommands::Unwind(cmd) => cmd.chain_spec(),
        }
    }
}

/// `base-node base-proofs` subcommands
#[derive(Debug, Subcommand)]
pub enum Subcommands {
    /// Initialize the proofs storage with the current state of the chain
    #[command(name = "init")]
    Init(init::InitCommand),
    /// Prune old proof history to reclaim space
    #[command(name = "prune")]
    Prune(prune::PruneCommand),
    /// Unwind the proofs storage to a specific block
    #[command(name = "unwind")]
    Unwind(unwind::UnwindCommand),
}
