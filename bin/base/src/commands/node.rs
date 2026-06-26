//! Execution-layer node maintenance commands.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    chainspec::BaseChainSpecParser,
    commands::{base_proofs, download, init_state, p2p},
};
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseExecutorProvider;
use base_node_core::BaseNode;
use clap::{Args, Subcommand};
use reth_cli_commands::{
    config_cmd, db,
    download::manifest_cmd::SnapshotManifestCommand,
    dump_genesis, init_cmd, prune, re_execute, stage,
};
use reth_cli_runner::CliRunner;

/// Execution-layer maintenance subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum NodeSubcommands {
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
    /// Manage storage of historical proofs in the fault-proof window.
    #[command(name = "proofs")]
    Proofs(base_proofs::Command<BaseChainSpecParser>),
    /// Generate modular chunk archives and a snapshot manifest.
    #[command(name = "snapshot-manifest")]
    SnapshotManifest(SnapshotManifestCommand),
    /// Download Base node snapshots from R2 storage.
    #[command(name = "download")]
    Download(download::BaseDownloadCommand<BaseChainSpecParser>),
}

/// Execution-layer node maintenance commands (`base node <subcommand>`).
#[derive(Args, Debug)]
pub(crate) struct NodeCommand {
    #[command(subcommand)]
    pub(crate) subcommand: NodeSubcommands,
}

impl NodeCommand {
    pub(crate) fn run(self) -> eyre::Result<()> {
        let runner = CliRunner::try_default_runtime()?;
        let components = |spec: Arc<BaseChainSpec>| {
            (
                BaseExecutorProvider::base(Arc::clone(&spec)),
                Arc::new(BaseBeaconConsensus::new(spec)),
            )
        };

        match self.subcommand {
            NodeSubcommands::Db(command) => {
                runner.run_blocking_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            NodeSubcommands::Init(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            NodeSubcommands::InitState(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            NodeSubcommands::DumpGenesis(command) => {
                runner.run_blocking_until_ctrl_c(command.execute())
            }
            NodeSubcommands::Stage(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode, _>(ctx, components))
            }
            NodeSubcommands::P2P(command) => runner.run_until_ctrl_c(command.execute::<BaseNode>()),
            NodeSubcommands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            NodeSubcommands::Prune(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode>(ctx))
            }
            NodeSubcommands::ReExecute(command) => {
                let runtime = runner.runtime();
                runner.run_until_ctrl_c(command.execute::<BaseNode>(components, runtime))
            }
            NodeSubcommands::Proofs(command) => {
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            NodeSubcommands::SnapshotManifest(command) => {
                command.execute()?;
                Ok(())
            }
            NodeSubcommands::Download(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>())
            }
        }
    }
}
