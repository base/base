//! Static Base L1 chain configuration mapping.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig as GenesisChainConfig;

use crate::{BaseMainnet, BaseSepolia};

pub(crate) fn l1_configs() -> impl ExactSizeIterator<Item = (u64, GenesisChainConfig)> {
    [
        (NamedChain::Base.into(), BaseMainnet::l1_config()),
        (NamedChain::BaseSepolia.into(), BaseSepolia::l1_config()),
    ]
    .into_iter()
}

#[cfg(test)]
mod tests {
    use alloy_primitives::map::HashMap;

    use super::*;

    #[test]
    fn base_l1_config_all_chains() {
        let base_chain_id = u64::from(NamedChain::Base);
        let base_sepolia_chain_id = u64::from(NamedChain::BaseSepolia);
        let configs = HashMap::<_, _>::from_iter(l1_configs());

        assert!(configs.get(&base_chain_id).is_some());
        assert!(configs.get(&base_sepolia_chain_id).is_some());
        assert!(configs.get(&99999).is_none());
    }
}
