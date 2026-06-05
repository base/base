//! Static Ethereum L1 chain configuration mapping.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig as GenesisChainConfig;

use crate::{Holesky, Hoodi, Mainnet, Sepolia};

pub(crate) fn l1_configs() -> impl ExactSizeIterator<Item = (u64, GenesisChainConfig)> {
    [
        (NamedChain::Mainnet.into(), Mainnet::l1_config()),
        (NamedChain::Sepolia.into(), Sepolia::l1_config()),
        (NamedChain::Holesky.into(), Holesky::l1_config()),
        (NamedChain::Hoodi.into(), Hoodi::l1_config()),
    ]
    .into_iter()
}

#[cfg(test)]
mod tests {
    use alloy_hardforks::{
        holesky::{HOLESKY_BPO1_TIMESTAMP, HOLESKY_BPO2_TIMESTAMP},
        sepolia::{SEPOLIA_BPO1_TIMESTAMP, SEPOLIA_BPO2_TIMESTAMP},
    };
    use alloy_primitives::map::HashMap;

    use super::*;

    #[test]
    fn l1_config_all_chains() {
        let mainnet_chain_id = u64::from(NamedChain::Mainnet);
        let sepolia_chain_id = u64::from(NamedChain::Sepolia);
        let holesky_chain_id = u64::from(NamedChain::Holesky);
        let hoodi_chain_id = u64::from(NamedChain::Hoodi);

        let configs = HashMap::<_, _>::from_iter(l1_configs());

        assert!(configs.get(&mainnet_chain_id).is_some());
        assert!(configs.get(&sepolia_chain_id).is_some());
        assert!(configs.get(&holesky_chain_id).is_some());
        assert!(configs.get(&hoodi_chain_id).is_some());
        assert!(configs.get(&99999).is_none());
    }

    #[test]
    fn bpo_timestamps() {
        let configs = HashMap::<_, _>::from_iter(l1_configs());

        let sepolia = configs.get(&11155111).unwrap();
        assert_eq!(sepolia.bpo1_time, Some(SEPOLIA_BPO1_TIMESTAMP));
        assert_eq!(sepolia.bpo2_time, Some(SEPOLIA_BPO2_TIMESTAMP));

        let holesky = configs.get(&17000).unwrap();
        assert_eq!(holesky.bpo1_time, Some(HOLESKY_BPO1_TIMESTAMP));
        assert_eq!(holesky.bpo2_time, Some(HOLESKY_BPO2_TIMESTAMP));
    }
}
