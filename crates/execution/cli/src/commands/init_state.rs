//! Command that initializes the node from a genesis file.

use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use clap::Parser;
use reth_cli_commands::ChainSpecParser;

/// Initializes the database with the genesis block.
#[derive(Debug, Parser)]
pub struct BaseInitStateCommand<C: ChainSpecParser> {
    #[command(flatten)]
    init_state: reth_cli_commands::init_state::InitStateCommand<C>,
}

impl<C: ChainSpecParser> BaseInitStateCommand<C> {
    /// Execute the `init` command
    pub async fn execute(self, runtime: base_common_runtime_tasks::Runtime) -> eyre::Result<()> {
        self.init_state.execute(runtime).await
    }
}

impl<C: ChainSpecParser> BaseInitStateCommand<C> {
    /// Returns the underlying chain being used to run this command.
    pub fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        self.init_state.chain_spec()
    }
}
