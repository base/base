//! Rollup and L1 chain configuration registry.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, map::HashMap};
use base_alloy_chains::BaseChainConfig;
use base_consensus_genesis::RollupConfig;
use spin::Lazy;

use crate::{Holesky, Hoodi, Mainnet, Sepolia};

/// Rollup configurations derived from [`BaseChainConfig`] instances.
static ROLLUP_CONFIGS: Lazy<HashMap<u64, RollupConfig>> = Lazy::new(|| {
    let mut map = HashMap::default();
    for cfg in BaseChainConfig::all() {
        map.insert(cfg.chain_id, RollupConfig::from(cfg));
    }
    map
});

/// L1 chain configurations built from known L1 genesis data.
static L1_CONFIGS: Lazy<HashMap<u64, ChainConfig>> = Lazy::new(|| {
    let mut map = HashMap::default();
    map.insert(NamedChain::Mainnet.into(), Mainnet::l1_config());
    map.insert(NamedChain::Sepolia.into(), Sepolia::l1_config());
    map.insert(NamedChain::Holesky.into(), Holesky::l1_config());
    map.insert(NamedChain::Hoodi.into(), Hoodi::l1_config());
    map
});

/// A registry of chain configurations for Base networks.
///
/// Provides access to rollup configs, L1 chain configs, and the unsafe block signer
/// for supported chain IDs. Rollup configs are derived from the compile-time
/// [`BaseChainConfig`] instances in `base-alloy-chains`.
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

    /// Returns an [`ChainConfig`] for the given L1 chain ID.
    pub fn l1_config(chain_id: u64) -> Option<&'static ChainConfig> {
        L1_CONFIGS.get(&chain_id)
    }

    /// Returns the `unsafe_block_signer` address for the given chain ID.
    pub fn unsafe_block_signer(chain_id: u64) -> Option<Address> {
        BaseChainConfig::by_chain_id(chain_id)?.unsafe_block_signer
    }
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain as AlloyChain;
    use alloy_hardforks::{
        holesky::{HOLESKY_BPO1_TIMESTAMP, HOLESKY_BPO2_TIMESTAMP},
        sepolia::{SEPOLIA_BPO1_TIMESTAMP, SEPOLIA_BPO2_TIMESTAMP},
    };
    use base_alloy_chains::BaseChainConfig;

    use super::*;

    #[test]
    fn test_unsafe_block_signer_mainnet() {
        let signer = Registry::unsafe_block_signer(8453).unwrap();
        assert_eq!(
            signer,
            "0xAf6E19BE0F9cE7f8afd49a1824851023A8249e8a".parse::<Address>().unwrap()
        );
    }

    #[test]
    fn test_unsafe_block_signer_sepolia() {
        let signer = Registry::unsafe_block_signer(84532).unwrap();
        assert_eq!(
            signer,
            "0xb830b99c95Ea32300039624Cb567d324D4b1D83C".parse::<Address>().unwrap()
        );
    }

    #[test]
    fn test_unsafe_block_signer_unknown_chain() {
        assert!(Registry::unsafe_block_signer(99999).is_none());
    }

    #[test]
    fn test_rollup_config_derived_from_chain_config() {
        let mainnet = Registry::rollup_config(8453).unwrap();
        let expected = RollupConfig::from(BaseChainConfig::mainnet());
        assert_eq!(*mainnet, expected);

        let sepolia = Registry::rollup_config(84532).unwrap();
        let expected = RollupConfig::from(BaseChainConfig::sepolia());
        assert_eq!(*sepolia, expected);
    }

    #[test]
    fn test_rollup_config_by_chain() {
        const ALLOY_BASE: AlloyChain = AlloyChain::base_mainnet();

        let by_chain = Registry::rollup_config_by_chain(&ALLOY_BASE).unwrap();
        let by_id = Registry::rollup_config(8453).unwrap();

        assert_eq!(by_chain, by_id);
    }

    #[test]
    fn test_jovian_timestamps() {
        let base_mainnet = Registry::rollup_config(8453).unwrap();
        assert_eq!(
            base_mainnet.hardforks.jovian_time,
            Some(BaseChainConfig::mainnet().jovian_timestamp)
        );

        let base_sepolia = Registry::rollup_config(84532).unwrap();
        assert_eq!(
            base_sepolia.hardforks.jovian_time,
            Some(BaseChainConfig::sepolia().jovian_timestamp)
        );
    }

    #[test]
    fn test_l1_config_all_chains() {
        // All four known L1 chains must be present.
        assert!(Registry::l1_config(NamedChain::Mainnet.into()).is_some());
        assert!(Registry::l1_config(NamedChain::Sepolia.into()).is_some());
        assert!(Registry::l1_config(NamedChain::Holesky.into()).is_some());
        assert!(Registry::l1_config(NamedChain::Hoodi.into()).is_some());
        // Unknown chain IDs must return None.
        assert!(Registry::l1_config(99999).is_none());
    }

    #[test]
    fn test_bpo_timestamps() {
        let sepolia_config = Registry::l1_config(11155111).unwrap();
        assert_eq!(sepolia_config.bpo1_time, Some(SEPOLIA_BPO1_TIMESTAMP));
        assert_eq!(sepolia_config.bpo2_time, Some(SEPOLIA_BPO2_TIMESTAMP));

        let holesky_config = Registry::l1_config(17000).unwrap();
        assert_eq!(holesky_config.bpo1_time, Some(HOLESKY_BPO1_TIMESTAMP));
        assert_eq!(holesky_config.bpo2_time, Some(HOLESKY_BPO2_TIMESTAMP));
    }
}
