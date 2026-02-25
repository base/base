//! Command that initializes the node from a genesis file.

use std::sync::Arc;

use base_execution_chainspec::OpChainSpec;
use base_execution_primitives::OpPrimitives;
use clap::Parser;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::CliNodeTypes;

/// Initializes the database with the genesis block.
#[derive(Debug, Parser)]
pub struct InitStateCommandOp<C: ChainSpecParser> {
    #[command(flatten)]
    init_state: reth_cli_commands::init_state::InitStateCommand<C>,
}

impl<C: ChainSpecParser<ChainSpec = OpChainSpec>> InitStateCommandOp<C> {
    /// Execute the `init` command
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = OpPrimitives>>(
        self,
    ) -> eyre::Result<()> {
        self.init_state.execute::<N>().await
    }
}

impl<C: ChainSpecParser> InitStateCommandOp<C> {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        self.init_state.chain_spec()
    }
}
