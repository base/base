//! Bootnodes for consensus network discovery.

use base_common_chains::ChainConfig;
use derive_more::Deref;

use crate::{BootNode, BootNodeParseError};

/// Bootnodes for Base.
#[derive(Debug, Clone, Deref, PartialEq, Eq, Default, derive_more::From)]
pub struct BootNodes(pub Vec<BootNode>);

impl TryFrom<&ChainConfig> for BootNodes {
    type Error = BootNodeParseError;

    fn try_from(config: &ChainConfig) -> Result<Self, Self::Error> {
        config
            .bootnodes
            .iter()
            .map(|raw| BootNode::parse_bootnode(raw))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

impl BootNodes {
    /// Returns the bootnodes for the given chain id.
    ///
    /// If the chain id is not recognized, no bootnodes are returned.
    pub fn from_chain_id(id: u64) -> Self {
        ChainConfig::by_chain_id(id)
            .map(|c| Self::try_from(c).expect("hardcoded bootnode should parse"))
            .unwrap_or_default()
    }

    /// Returns the bootnodes for the mainnet.
    pub fn mainnet() -> Self {
        Self::try_from(ChainConfig::mainnet()).expect("hardcoded bootnode should parse")
    }

    /// Returns the bootnodes for the testnet.
    pub fn testnet() -> Self {
        Self::try_from(ChainConfig::sepolia()).expect("hardcoded bootnode should parse")
    }

    /// Returns the length of the bootnodes.
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns if the bootnodes are empty.
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use base_common_chains::ChainConfig;

    use super::*;

    #[test]
    fn test_validate_bootnode_lens() {
        assert_eq!(ChainConfig::mainnet().bootnodes.len(), 10);
        assert_eq!(ChainConfig::sepolia().bootnodes.len(), 4);
        assert_eq!(ChainConfig::zeronet().bootnodes.len(), 4);
    }

    #[test]
    fn test_parse_raw_bootnodes() {
        for raw in ChainConfig::mainnet().bootnodes {
            BootNode::parse_bootnode(raw).expect("hardcoded bootnode should parse");
        }

        for raw in ChainConfig::sepolia().bootnodes {
            BootNode::parse_bootnode(raw).expect("hardcoded bootnode should parse");
        }

        for raw in ChainConfig::zeronet().bootnodes {
            BootNode::parse_bootnode(raw).expect("hardcoded bootnode should parse");
        }
    }

    #[test]
    fn test_bootnodes_from_chain_id() {
        let mainnet = BootNodes::from_chain_id(ChainConfig::mainnet().chain_id);
        assert_eq!(mainnet.len(), 10);

        let testnet = BootNodes::from_chain_id(ChainConfig::sepolia().chain_id);
        assert_eq!(testnet.len(), 4);

        let zeronet = BootNodes::from_chain_id(ChainConfig::zeronet().chain_id);
        assert_eq!(zeronet.len(), 4);

        let unknown = BootNodes::from_chain_id(0);
        assert!(unknown.is_empty());
    }

    #[test]
    fn test_bootnodes_len() {
        let bootnodes = BootNodes::mainnet();
        assert_eq!(bootnodes.len(), 10);

        let bootnodes = BootNodes::testnet();
        assert_eq!(bootnodes.len(), 4);
    }

    #[test]
    fn test_bootnodes_empty() {
        let bootnodes = BootNodes(vec![]);
        assert!(bootnodes.is_empty());

        let bootnodes = BootNodes::from_chain_id(ChainConfig::mainnet().chain_id);
        assert!(!bootnodes.is_empty());
    }
}
