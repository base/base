//! CLI subcommands for the execution layer node.

use std::{fmt, sync::Arc};

use base_execution_chainspec::BaseChainSpec;
use clap::Subcommand;
use reth_cli_commands::{
    config_cmd, db, dump_genesis, init_cmd,
    node::{self, NoArgs},
    prune, re_execute, stage,
};

use crate::chainspec::BaseChainSpecParser;

pub mod base_proofs;
pub mod download;
mod genesis_output_root;
pub use genesis_output_root::GenesisOutputRootCommand;
pub mod init_state;
pub mod p2p;
mod snapshot_manifest;
pub use snapshot_manifest::SnapshotManifestCommand;

/// Non-generic execution-layer maintenance commands dispatched outside [`crate::CliApp`] generics.
#[derive(Debug)]
#[non_exhaustive]
pub enum MaintenanceCommand {
    /// Initialize the database from a genesis file.
    Init(init_cmd::InitCommand<BaseChainSpecParser>),
    /// Initialize the database from a state dump file.
    InitState(init_state::BaseInitStateCommand<BaseChainSpecParser>),
    /// Dump genesis block JSON configuration to stdout.
    DumpGenesis(dump_genesis::DumpGenesisCommand<BaseChainSpecParser>),
    /// Print the OP Stack output root for an L2 genesis configuration.
    GenesisOutputRoot(GenesisOutputRootCommand),
    /// Database debugging utilities.
    Db(db::Command<BaseChainSpecParser>),
    /// Manipulate individual stages.
    Stage(Box<stage::Command<BaseChainSpecParser>>),
    /// P2P debugging utilities.
    P2P(Box<p2p::Command>),
    /// Write config to stdout.
    Config(config_cmd::Command),
    /// Prune according to the configuration without any limits.
    Prune(prune::PruneCommand<BaseChainSpecParser>),
    /// Generate test vectors.
    #[cfg(feature = "dev")]
    TestVectors(test_vectors::Command),
    /// Re-execute blocks in parallel to verify historical sync correctness.
    ReExecute(re_execute::Command<BaseChainSpecParser>),
    /// Manage storage of historical proofs in expanded trie db in fault proof window.
    BaseProofs(base_proofs::Command<BaseChainSpecParser>),
    /// Generate modular chunk archives and a snapshot manifest from a source datadir.
    SnapshotManifest(SnapshotManifestCommand),
    /// Download Base node snapshots from R2 storage.
    Download(download::BaseDownloadCommand<BaseChainSpecParser>),
}

/// Execution-layer maintenance subcommands re-exposed under `base reth`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub enum RethMaintenanceSubcommand {
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

impl From<RethMaintenanceSubcommand> for MaintenanceCommand {
    fn from(command: RethMaintenanceSubcommand) -> Self {
        match command {
            RethMaintenanceSubcommand::Db(cmd) => Self::Db(cmd),
            RethMaintenanceSubcommand::Init(cmd) => Self::Init(cmd),
            RethMaintenanceSubcommand::InitState(cmd) => Self::InitState(cmd),
            RethMaintenanceSubcommand::DumpGenesis(cmd) => Self::DumpGenesis(cmd),
            RethMaintenanceSubcommand::GenesisOutputRoot(cmd) => Self::GenesisOutputRoot(cmd),
            RethMaintenanceSubcommand::Stage(cmd) => Self::Stage(cmd),
            RethMaintenanceSubcommand::P2P(cmd) => Self::P2P(cmd),
            RethMaintenanceSubcommand::Config(cmd) => Self::Config(cmd),
            RethMaintenanceSubcommand::Prune(cmd) => Self::Prune(cmd),
            RethMaintenanceSubcommand::ReExecute(cmd) => Self::ReExecute(cmd),
        }
    }
}

#[cfg(feature = "dev")]
pub mod test_vectors;

/// Commands to be executed
#[derive(Debug, Subcommand)]
pub enum Commands<Ext: clap::Args + fmt::Debug = NoArgs> {
    /// Start the node
    #[command(name = "node")]
    Node(Box<node::NodeCommand<BaseChainSpecParser, Ext>>),
    /// Initialize the database from a genesis file.
    #[command(name = "init")]
    Init(init_cmd::InitCommand<BaseChainSpecParser>),
    /// Initialize the database from a state dump file.
    #[command(name = "init-state")]
    InitState(init_state::BaseInitStateCommand<BaseChainSpecParser>),
    /// Dumps genesis block JSON configuration to stdout.
    DumpGenesis(dump_genesis::DumpGenesisCommand<BaseChainSpecParser>),
    /// Database debugging utilities
    #[command(name = "db")]
    Db(db::Command<BaseChainSpecParser>),
    /// Manipulate individual stages.
    #[command(name = "stage")]
    Stage(Box<stage::Command<BaseChainSpecParser>>),
    /// P2P Debugging utilities
    #[command(name = "p2p")]
    P2P(Box<p2p::Command>),
    /// Write config to stdout
    #[command(name = "config")]
    Config(config_cmd::Command),
    /// Prune according to the configuration without any limits
    #[command(name = "prune")]
    Prune(prune::PruneCommand<BaseChainSpecParser>),
    /// Generate Test Vectors
    #[cfg(feature = "dev")]
    #[command(name = "test-vectors")]
    TestVectors(test_vectors::Command),
    /// Re-execute blocks in parallel to verify historical sync correctness.
    #[command(name = "re-execute")]
    ReExecute(re_execute::Command<BaseChainSpecParser>),
    /// Manage storage of historical proofs in expanded trie db in fault proof window.
    #[command(name = "proofs")]
    BaseProofs(base_proofs::Command<BaseChainSpecParser>),
    /// Generate modular chunk archives and a snapshot manifest from a source datadir.
    #[command(name = "snapshot-manifest")]
    SnapshotManifest(SnapshotManifestCommand),
    /// Download Base node snapshots from R2 storage.
    #[command(name = "download")]
    Download(download::BaseDownloadCommand<BaseChainSpecParser>),
}

impl<Ext: clap::Args + fmt::Debug> Commands<Ext> {
    /// Returns the underlying chain being used for commands
    pub fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        match self {
            Self::Node(cmd) => cmd.chain_spec(),
            Self::Init(cmd) => cmd.chain_spec(),
            Self::InitState(cmd) => cmd.chain_spec(),
            Self::DumpGenesis(cmd) => cmd.chain_spec(),
            Self::Db(cmd) => cmd.chain_spec(),
            Self::Stage(cmd) => cmd.chain_spec(),
            Self::P2P(cmd) => cmd.chain_spec(),
            Self::Config(_) => None,
            Self::Prune(cmd) => cmd.chain_spec(),
            #[cfg(feature = "dev")]
            Self::TestVectors(_) => None,
            Self::ReExecute(cmd) => cmd.chain_spec(),
            Self::BaseProofs(cmd) => cmd.chain_spec(),
            Self::SnapshotManifest(_) => None,
            Self::Download(cmd) => cmd.chain_spec(),
        }
    }

    /// Returns `true` if this is a node command with debug RPC namespace enabled.
    pub fn debug_namespace_enabled(&self) -> bool {
        match self {
            Self::Node(cmd) => {
                cmd.rpc.is_namespace_enabled(reth_rpc_server_types::RethRpcModule::Debug)
            }
            _ => false,
        }
    }

    /// Splits off the `node` command, the only variant generic over `Ext`.
    ///
    /// Every other command comes back as a non-generic [`MaintenanceCommand`] so its dispatch
    /// compiles once instead of once per `Ext` the CLI is instantiated with.
    pub fn split_node(
        self,
    ) -> Result<Box<node::NodeCommand<BaseChainSpecParser, Ext>>, Box<MaintenanceCommand>> {
        match self {
            Self::Node(cmd) => Ok(cmd),
            Self::Init(cmd) => Err(Box::new(MaintenanceCommand::Init(cmd))),
            Self::InitState(cmd) => Err(Box::new(MaintenanceCommand::InitState(cmd))),
            Self::DumpGenesis(cmd) => Err(Box::new(MaintenanceCommand::DumpGenesis(cmd))),
            Self::Db(cmd) => Err(Box::new(MaintenanceCommand::Db(cmd))),
            Self::Stage(cmd) => Err(Box::new(MaintenanceCommand::Stage(cmd))),
            Self::P2P(cmd) => Err(Box::new(MaintenanceCommand::P2P(cmd))),
            Self::Config(cmd) => Err(Box::new(MaintenanceCommand::Config(cmd))),
            Self::Prune(cmd) => Err(Box::new(MaintenanceCommand::Prune(cmd))),
            #[cfg(feature = "dev")]
            Self::TestVectors(cmd) => Err(Box::new(MaintenanceCommand::TestVectors(cmd))),
            Self::ReExecute(cmd) => Err(Box::new(MaintenanceCommand::ReExecute(cmd))),
            Self::BaseProofs(cmd) => Err(Box::new(MaintenanceCommand::BaseProofs(cmd))),
            Self::SnapshotManifest(cmd) => Err(Box::new(MaintenanceCommand::SnapshotManifest(cmd))),
            Self::Download(cmd) => Err(Box::new(MaintenanceCommand::Download(cmd))),
        }
    }
}
