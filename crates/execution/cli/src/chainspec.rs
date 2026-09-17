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
    use rstest::rstest;

    use super::*;

    #[test]
    fn parse_known_chain_spec() {
        for &chain in BaseChainSpecParser::SUPPORTED_CHAINS {
            assert!(
                <BaseChainSpecParser as ChainSpecParser>::parse(chain).is_ok(),
                "Failed to parse {chain}"
            );
        }
    }

    #[rstest]
    #[case("mainnet", 8453)]
    #[case("base", 8453)]
    #[case("sepolia", 84532)]
    #[case("base-sepolia", 84532)]
    #[case("base_sepolia", 84532)]
    #[case("zeronet", 763360)]
    #[case("base-zeronet", 763360)]
    #[case("dev", 84538453)]
    fn parse_chain_id(#[case] chain: &str, #[case] expected_id: u64) {
        let spec = BaseChainSpecParser::parse(chain).unwrap();
        assert_eq!(spec.chain().id(), expected_id);
    }
}
