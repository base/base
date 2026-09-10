//! `base reth` subcommand group: execution-layer maintenance utilities.

use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use base_common_cli::CliRunner;
use base_execution_evm_blocks::{BaseBeaconConsensus, BaseEvmConfig};
use base_node_cli::{
    CliNodeComponents, ConfigCommand, DbCommand, DumpGenesisCommand, InitCommand, PruneCommand,
    ReExecuteCommand, StageCommand,
    commands::{GenesisOutputRootCommand, p2p},
};
use clap::{Parser, Subcommand};

/// Execution-layer maintenance utilities for `base`.
#[derive(Parser, Debug)]
pub(crate) struct RethCommand {
    /// The reth-derived subcommand to execute.
    #[command(subcommand)]
    pub(crate) command: RethSubcommand,
}

impl RethCommand {
    pub(crate) fn run(self) -> eyre::Result<()> {
        self.command.run()
    }
}

/// Subcommands for `base reth`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub(crate) enum RethSubcommand {
    /// Database debugging utilities.
    #[command(name = "db")]
    Db(DbCommand),
    /// Initialize the database from a genesis file.
    #[command(name = "init")]
    Init(InitCommand),
    /// Initialize the database from a state dump file.
    #[command(name = "init-state")]
    InitState(base_node_cli::InitStateCommand),
    /// Dump genesis block JSON configuration to stdout.
    #[command(name = "dump-genesis")]
    DumpGenesis(DumpGenesisCommand),
    /// Print the OP Stack output root for an L2 genesis configuration.
    #[command(name = "genesis-output-root")]
    GenesisOutputRoot(GenesisOutputRootCommand),
    /// Manipulate individual stages.
    #[command(name = "stage")]
    Stage(Box<StageCommand>),
    /// P2P debugging utilities.
    #[command(name = "p2p")]
    P2P(Box<p2p::Command>),
    /// Write config to stdout.
    #[command(name = "config")]
    Config(ConfigCommand),
    /// Prune according to the configuration without any limits.
    #[command(name = "prune")]
    Prune(PruneCommand),
    /// Re-execute blocks in parallel to verify historical sync correctness.
    #[command(name = "re-execute")]
    ReExecute(ReExecuteCommand),
}

impl RethSubcommand {
    pub(crate) fn run(self) -> eyre::Result<()> {
        match self {
            Self::Db(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_blocking_command_until_exit(|ctx| command.execute(ctx))
            }
            Self::Init(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute(runtime))
            }
            Self::InitState(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute(runtime))
            }
            Self::DumpGenesis(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_blocking_until_ctrl_c(command.execute())
            }
            Self::GenesisOutputRoot(command) => {
                command.execute();
                Ok(())
            }
            Self::Stage(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_command_until_exit(|ctx| command.execute(ctx, Self::base_components))
            }
            Self::P2P(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_until_ctrl_c(command.execute())
            }
            Self::Config(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_until_ctrl_c(command.execute())
            }
            Self::Prune(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_command_until_exit(|ctx| command.execute(ctx))
            }
            Self::ReExecute(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_until_ctrl_c(command.execute(Self::base_components, runtime))
            }
        }
    }

    pub(crate) fn base_components(spec: Arc<BaseChainSpec>) -> CliNodeComponents {
        CliNodeComponents {
            evm_config: BaseEvmConfig::new(Arc::clone(&spec)),
            consensus: Arc::new(BaseBeaconConsensus::new(spec)),
        }
    }
}
