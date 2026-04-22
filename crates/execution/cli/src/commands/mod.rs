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

pub mod init_state;
pub mod op_proofs;
pub mod p2p;

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
    InitState(init_state::InitStateCommandOp<BaseChainSpecParser>),
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
    BaseProofs(op_proofs::Command<BaseChainSpecParser>),
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
        }
    }
}
