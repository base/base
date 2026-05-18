//! Static Ethereum L1 chain configuration mapping.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig as GenesisChainConfig;
use alloy_primitives::map::HashMap;
use spin::Lazy;

use crate::{Holesky, Hoodi, Mainnet, Sepolia};

/// Ethereum L1 chain configurations keyed by chain ID.
pub static L1_CONFIGS: Lazy<HashMap<u64, GenesisChainConfig>> = Lazy::new(|| {
    let mut map = HashMap::default();
    map.insert(NamedChain::Mainnet.into(), Mainnet::l1_config());
    map.insert(NamedChain::Sepolia.into(), Sepolia::l1_config());
    map.insert(NamedChain::Holesky.into(), Holesky::l1_config());
    map.insert(NamedChain::Hoodi.into(), Hoodi::l1_config());
    map
});

#[cfg(test)]
mod tests {
    use alloy_hardforks::{
        holesky::{HOLESKY_BPO1_TIMESTAMP, HOLESKY_BPO2_TIMESTAMP},
        sepolia::{SEPOLIA_BPO1_TIMESTAMP, SEPOLIA_BPO2_TIMESTAMP},
    };

    use super::*;

    #[test]
    fn l1_config_all_chains() {
        let mainnet_chain_id = u64::from(NamedChain::Mainnet);
        let sepolia_chain_id = u64::from(NamedChain::Sepolia);
        let holesky_chain_id = u64::from(NamedChain::Holesky);
        let hoodi_chain_id = u64::from(NamedChain::Hoodi);

        assert!(L1_CONFIGS.get(&mainnet_chain_id).is_some());
        assert!(L1_CONFIGS.get(&sepolia_chain_id).is_some());
        assert!(L1_CONFIGS.get(&holesky_chain_id).is_some());
        assert!(L1_CONFIGS.get(&hoodi_chain_id).is_some());
        assert!(L1_CONFIGS.get(&99999).is_none());
    }

    #[test]
    fn bpo_timestamps() {
        let sepolia = L1_CONFIGS.get(&11155111).unwrap();
        assert_eq!(sepolia.bpo1_time, Some(SEPOLIA_BPO1_TIMESTAMP));
        assert_eq!(sepolia.bpo2_time, Some(SEPOLIA_BPO2_TIMESTAMP));

        let holesky = L1_CONFIGS.get(&17000).unwrap();
        assert_eq!(holesky.bpo1_time, Some(HOLESKY_BPO1_TIMESTAMP));
        assert_eq!(holesky.bpo2_time, Some(HOLESKY_BPO2_TIMESTAMP));
    }
}
