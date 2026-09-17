use std::sync::Arc;

use base_common_chains::ChainConfig;
use base_execution_chainspec::BaseChainSpec;
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};

/// Base chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BaseChainSpecParser;

impl ChainSpecParser for BaseChainSpecParser {
    type ChainSpec = BaseChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = ChainConfig::SUPPORTED_NAMES;

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

/// Clap value parser for [`BaseChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<BaseChainSpec>, eyre::Error> {
    if let Some(base_chain_spec) = BaseChainSpec::parse_chain(s) {
        Ok(base_chain_spec)
    } else {
        Ok(Arc::new(BaseChainSpec::try_from_genesis(parse_genesis(s)?)?))
    }
}

#[cfg(test)]
mod tests {
    use reth_chainspec::EthChainSpec;

    use super::*;

    #[test]
    fn parses_chain_names_to_expected_ids() {
        for (chain, expected_id) in [
            ("mainnet", 8453),
            ("base", 8453),
            ("sepolia", 84532),
            ("base-sepolia", 84532),
            ("base_sepolia", 84532),
            ("zeronet", 763360),
            ("base-zeronet", 763360),
            ("dev", 84538453),
        ] {
            let spec = <BaseChainSpecParser as ChainSpecParser>::parse(chain)
                .unwrap_or_else(|error| panic!("failed to parse {chain}: {error}"));
            assert_eq!(spec.chain().id(), expected_id, "unexpected chain ID for {chain}");
        }
    }
}
