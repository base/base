//! Rollup chain configuration registry.

use alloy_primitives::{Address, map::HashMap};
use base_common_genesis::RollupConfig;
use spin::Lazy;

use crate::ChainConfig;

/// Rollup configurations derived from [`ChainConfig`] instances.
static ROLLUP_CONFIGS: Lazy<HashMap<u64, RollupConfig>> = Lazy::new(|| {
    let mut map = HashMap::default();
    for cfg in ChainConfig::all() {
        map.insert(cfg.chain_id, cfg.rollup_config());
    }
    map
});

/// A registry of chain configurations for Base networks.
///
/// Provides access to rollup configs and the unsafe block signer for supported chain IDs.
/// Rollup configs are derived from the compile-time [`ChainConfig`] instances in this crate.
#[derive(Debug)]
pub struct Registry;

impl Registry {
    /// Returns a [`RollupConfig`] for the given chain ID.
    pub fn rollup_config(chain_id: u64) -> Option<&'static RollupConfig> {
        ROLLUP_CONFIGS.get(&chain_id)
    }

    /// Returns a [`RollupConfig`] by its [`alloy_chains::Chain`] identifier.
    pub fn rollup_config_by_chain(chain: &alloy_chains::Chain) -> Option<&'static RollupConfig> {
        ROLLUP_CONFIGS.get(&chain.id())
    }

    /// Returns the `unsafe_block_signer` address for the given chain ID.
    pub fn unsafe_block_signer(chain_id: u64) -> Option<Address> {
        ChainConfig::by_chain_id(chain_id)?.unsafe_block_signer
    }
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain as AlloyChain;

    use super::*;

    #[test]
    fn unsafe_block_signer_mainnet() {
        let signer = Registry::unsafe_block_signer(8453).unwrap();
        assert_eq!(
            signer,
            "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a".parse::<Address>().unwrap()
        );
    }

    #[test]
    fn unsafe_block_signer_sepolia() {
        let signer = Registry::unsafe_block_signer(84532).unwrap();
        assert_eq!(
            signer,
            "0xb830b99c95Ea32300039624Cb567d324D4b1D83C".parse::<Address>().unwrap()
        );
    }

    #[test]
    fn unsafe_block_signer_unknown_chain() {
        assert!(Registry::unsafe_block_signer(99999).is_none());
    }

    #[test]
    fn rollup_config_derived_from_chain_config() {
        let mainnet = Registry::rollup_config(8453).unwrap();
        assert_eq!(*mainnet, ChainConfig::mainnet().rollup_config());

        let sepolia = Registry::rollup_config(84532).unwrap();
        assert_eq!(*sepolia, ChainConfig::sepolia().rollup_config());
    }

    #[test]
    fn rollup_config_by_chain() {
        const ALLOY_BASE: AlloyChain = AlloyChain::base_mainnet();

        let by_chain = Registry::rollup_config_by_chain(&ALLOY_BASE).unwrap();
        let by_id = Registry::rollup_config(8453).unwrap();

        assert_eq!(by_chain, by_id);
    }

    #[test]
    fn jovian_timestamps() {
        let base_mainnet = Registry::rollup_config(8453).unwrap();
        assert_eq!(
            base_mainnet.hardforks.jovian_time,
            Some(ChainConfig::mainnet().jovian_timestamp)
        );

        let base_sepolia = Registry::rollup_config(84532).unwrap();
        assert_eq!(
            base_sepolia.hardforks.jovian_time,
            Some(ChainConfig::sepolia().jovian_timestamp)
        );
    }
}
