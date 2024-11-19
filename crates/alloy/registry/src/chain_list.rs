//! List of OP Stack chains.

use alloc::{string::String, vec::Vec};

/// List of Chains.
#[derive(Debug, Clone, Default, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChainList {
    /// List of Chains.
    pub chains: Vec<Chain>,
}

/// A Chain Definition.
#[derive(Debug, Clone, Default, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chain {
    /// The name of the chain.
    pub name: String,
    /// Chain identifier.
    pub identifier: String,
    /// Chain ID.
    pub chain_id: u64,
    /// List of RPC Endpoints.
    pub rpc: Vec<String>,
    /// List of Explorer Endpoints.
    pub explorers: Vec<String>,
    /// The Superchain Level.
    pub superchain_level: u64,
    /// The data avilability type.
    pub data_availability_type: String,
    /// The Superchain Parent.
    pub parent: SuperchainParent,
}

/// A Chain Parent
#[derive(Debug, Clone, Default, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuperchainParent {
    /// The parent type.
    pub r#type: String,
    /// The chain identifier.
    pub chain: String,
}

impl SuperchainParent {
    /// Returns the chain id for the parent.
    pub fn chain_id(&self) -> u64 {
        match self.chain.as_ref() {
            "mainnet" => 1,
            "sepolia" => 11155111,
            "sepolia-dev-0" => 11155421,
            _ => 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_chain_list_file() {
        let chain_list = include_str!("../etc/chainList.json");
        let chains: Vec<Chain> = serde_json::from_str(chain_list).unwrap();
        let base_chain = chains.iter().find(|c| c.name == "Base").unwrap();
        assert_eq!(base_chain.chain_id, 8453);
    }
}
