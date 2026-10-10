//! `base reth` subcommand group: execution-layer maintenance utilities.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    chainspec::BaseChainSpecParser,
    commands::{GenesisOutputRootCommand, init_state, p2p},
};
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseExecutorProvider;
use base_node_core::BaseNode;
use clap::{ArgMatches, Args, Command, FromArgMatches, Subcommand, parser::ValueSource};
use reth_cli_commands::{config_cmd, db, dump_genesis, init_cmd, prune, re_execute, stage};
use reth_cli_runner::CliRunner;

/// Execution-layer maintenance utilities re-exposed from `base-reth-node`.
#[derive(Debug)]
pub(crate) struct RethCommand {
    /// The reth-derived subcommand to execute.
    pub(crate) command: RethSubcommand,
    /// Whether the subcommand was given its own `--metrics` endpoint, which installs reth's
    /// global Prometheus recorder when it serves its first scrape.
    pub(crate) serves_reth_metrics: bool,
}

impl RethCommand {
    /// The long name of the endpoint flag reth's maintenance subcommands share.
    pub(crate) const METRICS_FLAG: &str = "metrics";

    /// Returns whether the subcommand selected in `matches` was given [`Self::METRICS_FLAG`].
    ///
    /// The subcommands keep that argument private, so it is read from the parsed matches.
    pub(crate) fn requests_metrics_endpoint(command: &Command, matches: &ArgMatches) -> bool {
        if let Some((name, matches)) = matches.subcommand() {
            return command
                .find_subcommand(name)
                .is_some_and(|command| Self::requests_metrics_endpoint(command, matches));
        }
        command
            .get_arguments()
            .find(|arg| arg.get_long() == Some(Self::METRICS_FLAG))
            .and_then(|arg| matches.value_source(arg.get_id().as_str()))
            .is_some_and(|source| source != ValueSource::DefaultValue)
    }
}

impl Args for RethCommand {
    fn augment_args(command: Command) -> Command {
        RethSubcommand::augment_subcommands(command)
            .subcommand_required(true)
            .arg_required_else_help(true)
    }

    fn augment_args_for_update(command: Command) -> Command {
        RethSubcommand::augment_subcommands_for_update(command)
            .subcommand_required(true)
            .arg_required_else_help(true)
    }
}

impl FromArgMatches for RethCommand {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        Ok(Self {
            command: RethSubcommand::from_arg_matches(matches)?,
            serves_reth_metrics: Self::requests_metrics_endpoint(
                &Self::augment_args(Command::new("reth")),
                matches,
            ),
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

/// Subcommands for `base reth`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub(crate) enum RethSubcommand {
    /// Database debugging utilities.
    #[command(name = "db")]
    Db(db::Command<BaseChainSpecParser>),
    /// Initialize the database from a genesis file.
    #[command(name = "init")]
    Init(init_cmd::InitCommand<BaseChainSpecParser>),
    /// Initialize the database from a state dump file.
    #[command(name = "init-state")]
    InitState(init_state::BaseInitStateCommand<BaseChainSpecParser>),
    /// Dump genesis block JSON configuration to stdout.
    #[command(name = "dump-genesis")]
    DumpGenesis(dump_genesis::DumpGenesisCommand<BaseChainSpecParser>),
    /// Print the OP Stack output root for an L2 genesis configuration.
    #[command(name = "genesis-output-root")]
    GenesisOutputRoot(GenesisOutputRootCommand),
    /// Manipulate individual stages.
    #[command(name = "stage")]
    Stage(Box<stage::Command<BaseChainSpecParser>>),
    /// P2P debugging utilities.
    #[command(name = "p2p")]
    P2P(Box<p2p::Command>),
    /// Write config to stdout.
    #[command(name = "config")]
    Config(config_cmd::Command),
    /// Prune according to the configuration without any limits.
    #[command(name = "prune")]
    Prune(prune::PruneCommand<BaseChainSpecParser>),
    /// Re-execute blocks in parallel to verify historical sync correctness.
    #[command(name = "re-execute")]
    ReExecute(re_execute::Command<BaseChainSpecParser>),
}

impl RethSubcommand {
    pub(crate) fn run(self) -> eyre::Result<()> {
        match self {
            Self::Db(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_blocking_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            Self::Init(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Self::InitState(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
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
                runner.run_command_until_exit(|ctx| {
                    command.execute::<BaseNode, _>(ctx, Self::base_components)
                })
            }
            Self::P2P(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_until_ctrl_c(command.execute::<BaseNode>())
            }
            Self::Config(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_until_ctrl_c(command.execute())
            }
            Self::Prune(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            Self::ReExecute(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_until_ctrl_c(command.execute::<BaseNode>(Self::base_components, runtime))
            }
        }
    }

    pub(crate) fn base_components(
        spec: Arc<BaseChainSpec>,
    ) -> (BaseExecutorProvider, Arc<BaseBeaconConsensus>) {
        (BaseExecutorProvider::base(Arc::clone(&spec)), Arc::new(BaseBeaconConsensus::new(spec)))
    }
}
