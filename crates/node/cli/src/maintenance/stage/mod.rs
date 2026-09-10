//! `reth stage` command

use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use base_common_cli_support::CliContext;
use clap::{Parser, Subcommand};

use crate::CliNodeComponents;

mod drop;
pub use drop::Command as DropCommand;
mod dump;
pub use dump::{Command as DumpCommand, StageCommand as DumpStageCommand, Stages};
mod run;
pub use run::Command as RunCommand;
mod unwind;
pub use unwind::Command as UnwindCommand;

/// `reth stage` command
#[derive(Debug, Parser)]
pub struct Command {
    #[command(subcommand)]
    pub command: Subcommands,
}

/// `reth stage` subcommands
#[derive(Subcommand, Debug)]
pub enum Subcommands {
    /// Run a single stage.
    ///
    /// Note that this won't use the Pipeline and as a result runs stages
    /// assuming that all the data can be held in memory. It is not recommended
    /// to run a stage for really large block ranges if your computer does not have
    /// a lot of memory to store all the data.
    Run(Box<run::Command>),
    /// Drop a stage's tables from the database.
    Drop(drop::Command),
    /// Dumps a stage from a range into a new database.
    Dump(dump::Command),
    /// Unwinds a certain block range, deleting it from the database.
    Unwind(unwind::Command),
}

impl Command {
    /// Execute `stage` command
    pub async fn execute(
        self,
        ctx: CliContext,
        components: impl FnOnce(Arc<BaseChainSpec>) -> CliNodeComponents,
    ) -> eyre::Result<()> {
        let executor = ctx.task_executor.clone();
        match self.command {
            Subcommands::Run(command) => command.execute::<_>(ctx, components).await,
            Subcommands::Drop(command) => command.execute(executor).await,
            Subcommands::Dump(command) => command.execute::<_>(components, executor).await,
            Subcommands::Unwind(command) => command.execute::<_>(components, executor).await,
        }
    }
}

impl Command {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        match self.command {
            Subcommands::Run(ref command) => command.chain_spec(),
            Subcommands::Drop(ref command) => command.chain_spec(),
            Subcommands::Dump(ref command) => command.chain_spec(),
            Subcommands::Unwind(ref command) => command.chain_spec(),
        }
    }
}
