//! Command that prints the OP Stack output root for an L2 genesis configuration.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use clap::Parser;

use crate::chainspec::chain_value_parser;

/// Prints the V0 output root for a Base genesis configuration.
#[derive(Debug, Parser)]
pub struct GenesisOutputRootCommand {
    /// Built-in chain name or path to a genesis JSON file.
    #[arg(long, value_parser = chain_value_parser)]
    pub chain: Arc<BaseChainSpec>,
}

impl GenesisOutputRootCommand {
    /// Computes and prints the genesis output root.
    pub fn execute(self) {
        println!("{}", self.chain.genesis_output_root());
    }
}
