use std::sync::Arc;

use base_common_chain_config::{BaseChainSpec, ChainConfig};
use base_common_cli::parse_genesis;
use clap::builder::TypedValueParser;

use crate::ChainSpecValueParser;

/// Base chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BaseChainSpecParser;

impl BaseChainSpecParser {
    /// Built-in Base chains accepted by maintenance commands.
    pub const SUPPORTED_CHAINS: &'static [&'static str] = ChainConfig::SUPPORTED_NAMES;

    /// Parses a built-in Base chain or genesis JSON.
    pub fn parse(s: &str) -> eyre::Result<Arc<BaseChainSpec>> {
        chain_value_parser(s)
    }
    /// Default chain selected by maintenance commands.
    pub fn default_value() -> Option<&'static str> {
        Self::SUPPORTED_CHAINS.first().copied()
    }

    /// Clap parser for Base chain specifications.
    pub const fn parser() -> impl TypedValueParser<Value = Arc<BaseChainSpec>> {
        ChainSpecValueParser
    }

    /// Help text describing supported chain inputs.
    pub fn help_message() -> String {
        format!(
            "The chain this node is running.\nPossible values are either a built-in chain or the path to a chain specification file.\n\nBuilt-in chains:\n    {}",
            Self::SUPPORTED_CHAINS.join(", ")
        )
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
    use super::*;

    #[test]
    fn parse_known_chain_spec() {
        for &chain in BaseChainSpecParser::SUPPORTED_CHAINS {
            assert!(BaseChainSpecParser::parse(chain).is_ok(), "Failed to parse {chain}");
        }
    }
}
