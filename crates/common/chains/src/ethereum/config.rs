//! Static Ethereum L1 chain configuration mapping.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig as GenesisChainConfig;
use alloy_primitives::map::HashMap;
use spin::Lazy;

use crate::{Devnet, Holesky, Hoodi, Mainnet, Sepolia};

/// Ethereum L1 chain configurations keyed by chain ID.
pub static L1_CONFIGS: Lazy<HashMap<u64, GenesisChainConfig>> = Lazy::new(|| {
    let mut map = HashMap::default();
    map.insert(Devnet::CHAIN_ID, Devnet::l1_config());
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
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::devnet(Devnet::CHAIN_ID, true)]
    #[case::mainnet(u64::from(NamedChain::Mainnet), true)]
    #[case::sepolia(u64::from(NamedChain::Sepolia), true)]
    #[case::holesky(u64::from(NamedChain::Holesky), true)]
    #[case::hoodi(u64::from(NamedChain::Hoodi), true)]
    #[case::unknown_chain(99999, false)]
    fn l1_config_all_chains(#[case] chain_id: u64, #[case] expected: bool) {
        assert_eq!(L1_CONFIGS.contains_key(&chain_id), expected);
    }

    #[test]
    fn devnet_rollup_l1_config_resolves() {
        let rollup_config = crate::rollup_config!(crate::ChainConfig::devnet().chain_id).unwrap();
        let l1_config = L1_CONFIGS.get(&rollup_config.l1_chain_id).unwrap();

        assert_eq!(l1_config.chain_id, Devnet::CHAIN_ID);
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
