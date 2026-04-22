//! Lookup table for known L1 chain configurations.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig as GenesisChainConfig;
use alloy_primitives::map::HashMap;
use spin::Lazy;

use crate::{Holesky, Hoodi, Mainnet, Sepolia};

/// L1 chain configurations built from known L1 genesis data.
static L1_CONFIGS: Lazy<HashMap<u64, GenesisChainConfig>> = Lazy::new(|| {
    let mut map = HashMap::default();
    map.insert(NamedChain::Mainnet.into(), Mainnet::l1_config());
    map.insert(NamedChain::Sepolia.into(), Sepolia::l1_config());
    map.insert(NamedChain::Holesky.into(), Holesky::l1_config());
    map.insert(NamedChain::Hoodi.into(), Hoodi::l1_config());
    map
});

/// Returns the [`GenesisChainConfig`] for the given L1 chain ID, if known.
pub fn l1_config(chain_id: u64) -> Option<&'static GenesisChainConfig> {
    L1_CONFIGS.get(&chain_id)
}

#[cfg(test)]
mod tests {
    use alloy_hardforks::{
        holesky::{HOLESKY_BPO1_TIMESTAMP, HOLESKY_BPO2_TIMESTAMP},
        sepolia::{SEPOLIA_BPO1_TIMESTAMP, SEPOLIA_BPO2_TIMESTAMP},
    };

    use super::*;

    #[test]
    fn l1_config_all_chains() {
        assert!(l1_config(NamedChain::Mainnet.into()).is_some());
        assert!(l1_config(NamedChain::Sepolia.into()).is_some());
        assert!(l1_config(NamedChain::Holesky.into()).is_some());
        assert!(l1_config(NamedChain::Hoodi.into()).is_some());
        assert!(l1_config(99999).is_none());
    }

    #[test]
    fn bpo_timestamps() {
        let sepolia = l1_config(11155111).unwrap();
        assert_eq!(sepolia.bpo1_time, Some(SEPOLIA_BPO1_TIMESTAMP));
        assert_eq!(sepolia.bpo2_time, Some(SEPOLIA_BPO2_TIMESTAMP));

        let holesky = l1_config(17000).unwrap();
        assert_eq!(holesky.bpo1_time, Some(HOLESKY_BPO1_TIMESTAMP));
        assert_eq!(holesky.bpo2_time, Some(HOLESKY_BPO2_TIMESTAMP));
    }
}
