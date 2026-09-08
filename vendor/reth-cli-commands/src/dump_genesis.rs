//! Command that dumps genesis block JSON configuration to stdout
use std::sync::Arc;

use base_cli_utils::ChainSpecParser;
use base_execution_chainspec::BaseChainSpec;
use clap::Parser;

/// Dumps genesis block JSON configuration to stdout
#[derive(Debug, Parser)]
pub struct DumpGenesisCommand<C: ChainSpecParser> {
    /// Parser used for built-in chain names and genesis files.
    #[arg(skip)]
    pub parser: core::marker::PhantomData<C>,
    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = C::help_message(),
        default_value = C::default_value(),
        value_parser = C::parser()
    )]
    chain: Arc<BaseChainSpec>,
}

impl<C: ChainSpecParser> DumpGenesisCommand<C> {
    /// Execute the `dump-genesis` command
    pub async fn execute(self) -> eyre::Result<()> {
        println!("{}", serde_json::to_string_pretty(self.chain.genesis())?);
        Ok(())
    }
}

impl<C: ChainSpecParser> DumpGenesisCommand<C> {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        Some(&self.chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{BaseTestChainSpecParser, SUPPORTED_CHAINS};

    #[test]
    fn parse_dump_genesis_command_chain_args() {
        for (chain, expected_id) in SUPPORTED_CHAINS.iter().zip([8453, 84532, 84538453, 763360]) {
            let args: DumpGenesisCommand<BaseTestChainSpecParser> =
                DumpGenesisCommand::parse_from(["reth", "--chain", chain]);
            assert_eq!(
                Ok(args.chain.chain()),
                Ok::<_, ()>(alloy_chains::Chain::from_id(expected_id)),
                "failed to parse chain {chain}"
            );
        }
    }
}
