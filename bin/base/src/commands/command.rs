//! Top-level command dispatch for the unified Base binary.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    chainspec::BaseChainSpecParser,
    commands::{base_proofs, download, init_state, p2p},
};
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseExecutorProvider;
use base_node_core::BaseNode;
use clap::Subcommand;
use reth_cli_commands::{
    config_cmd, db,
    download::manifest_cmd::SnapshotManifestCommand,
    dump_genesis, init_cmd, prune, re_execute, stage,
};
use reth_cli_runner::CliRunner;

use crate::{
    commands::{bootnode::BootnodeCommand, rpc::RpcCommand, sequencer::SequencerCommand, update::UpdateCommand},
    config::ChainResolver,
};

/// Top-level commands for `base`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub(crate) enum BaseCommand {
    /// Run consensus and execution discovery-only bootnodes.
    #[command(name = "bootnode")]
    Bootnode(Box<BootnodeCommand>),
    /// Run the integrated node in RPC mode.
    #[command(name = "rpc")]
    Rpc(Box<RpcCommand>),
    /// Run integrated execution, builder, and consensus services in sequencer mode.
    #[command(name = "sequencer")]
    Sequencer(Box<SequencerCommand>),
    /// Update the base binary to the latest release.
    #[command(name = "update")]
    Update(Box<UpdateCommand>),
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

impl BaseCommand {
    pub(crate) fn run(
        self,
        chain_resolver: ChainResolver,
        metrics_enabled: bool,
    ) -> eyre::Result<()> {
        match self {
            Self::Bootnode(bootnode) => (*bootnode).run(chain_resolver.resolve()?, metrics_enabled),
            Self::Rpc(rpc) => (*rpc).run(chain_resolver.resolve()?, metrics_enabled),
            Self::Sequencer(sequencer) => {
                (*sequencer).run(chain_resolver.resolve()?, metrics_enabled)
            }
            Self::Update(update) => (*update).run(),
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
            Self::Stage(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let components = |spec: Arc<BaseChainSpec>| {
                    (
                        BaseExecutorProvider::base(Arc::clone(&spec)),
                        Arc::new(BaseBeaconConsensus::new(spec)),
                    )
                };
                runner.run_command_until_exit(|ctx| command.execute::<BaseNode, _>(ctx, components))
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
                let components = |spec: Arc<BaseChainSpec>| {
                    (
                        BaseExecutorProvider::base(Arc::clone(&spec)),
                        Arc::new(BaseBeaconConsensus::new(spec)),
                    )
                };
                runner.run_until_ctrl_c(command.execute::<BaseNode>(components, runtime))
            }
            Self::Proofs(command) => {
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>(runtime))
            }
            Self::SnapshotManifest(command) => {
                command.execute()?;
                Ok(())
            }
            Self::Download(command) => {
                let runner = CliRunner::try_default_runtime()?;
                runner.run_blocking_until_ctrl_c(command.execute::<BaseNode>())
            }
        }
    }
}
