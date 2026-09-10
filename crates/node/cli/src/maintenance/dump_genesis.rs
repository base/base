//! Command that dumps genesis block JSON configuration to stdout
use std::sync::Arc;

use base_common_chain_config::BaseChainSpec;
use clap::Parser;

/// Dumps genesis block JSON configuration to stdout
#[derive(Debug, Parser)]
pub struct DumpGenesisCommand {
    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = crate::BaseChainSpecParser::help_message(),
        default_value = crate::BaseChainSpecParser::default_value(),
        value_parser = crate::BaseChainSpecParser::parser()
    )]
    chain: Arc<BaseChainSpec>,
}

impl DumpGenesisCommand {
    /// Execute the `dump-genesis` command
    pub async fn execute(self) -> eyre::Result<()> {
        println!("{}", serde_json::to_string_pretty(self.chain.genesis())?);
        Ok(())
    }
}

impl DumpGenesisCommand {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        Some(&self.chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dump_genesis_command_chain_args() {
        for (chain, expected_id) in [
            ("base", 8453),
            ("base_sepolia", 84532),
            ("base-sepolia", 84532),
            ("base-zeronet", 763360),
            ("dev", 84538453),
        ] {
            let args: DumpGenesisCommand =
                DumpGenesisCommand::parse_from(["reth", "--chain", chain]);
            assert_eq!(
                Ok(args.chain.chain()),
                Ok::<_, ()>(alloy_chains::Chain::from_id(expected_id)),
                "failed to parse chain {chain}"
            );
        }
    }
}
