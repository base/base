//! Base Sepolia testnet L1 chain configuration.

use alloy_chains::NamedChain;
use alloy_genesis::ChainConfig;
use alloy_primitives::U256;

/// Base Sepolia testnet L1 chain configuration builder.
#[derive(Debug, Clone, Copy)]
pub struct BaseSepolia;

impl BaseSepolia {
    /// Canyon activation timestamp, inherited from the OP Sepolia superchain schedule.
    /// Maps to the Shanghai execution-layer fork.
    const CANYON_TIME: u64 = 1_699_981_200;
    /// Ecotone activation timestamp, inherited from the OP Sepolia superchain schedule.
    /// Maps to the Cancun execution-layer fork.
    const ECOTONE_TIME: u64 = 1_708_534_800;
    /// Isthmus activation timestamp, inherited from the OP Sepolia superchain schedule.
    /// Maps to the Prague execution-layer fork.
    const ISTHMUS_TIME: u64 = 1_744_905_600;

    /// Returns the Base Sepolia testnet [`ChainConfig`].
    ///
    /// Base is an OP Stack chain: it is post-merge from genesis, so every pre-merge fork is
    /// active at block 0, there is no proof-of-work stage, and there is no beacon deposit
    /// contract. The time-based forks are pinned to the OP upgrade activations that bring the
    /// equivalent execution-layer features (Canyon=>Shanghai, Ecotone=>Cancun, Isthmus=>Prague).
    pub fn l1_config() -> ChainConfig {
        ChainConfig {
            chain_id: NamedChain::BaseSepolia.into(),
            homestead_block: Some(0),
            dao_fork_block: Some(0),
            dao_fork_support: false,
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            muir_glacier_block: Some(0),
            berlin_block: Some(0),
            london_block: Some(0),
            arrow_glacier_block: Some(0),
            gray_glacier_block: Some(0),
            shanghai_time: Some(Self::CANYON_TIME),
            cancun_time: Some(Self::ECOTONE_TIME),
            prague_time: Some(Self::ISTHMUS_TIME),
            osaka_time: None,
            amsterdam_time: None,
            bpo1_time: None,
            bpo2_time: None,
            bpo3_time: None,
            bpo4_time: None,
            bpo5_time: None,
            ethash: None,
            blob_schedule: super::BlobSchedule::schedule(),
            merge_netsplit_block: Some(0),
            terminal_total_difficulty: Some(U256::ZERO),
            deposit_contract_address: None,
            clique: None,
            parlia: None,
            extra_fields: Default::default(),
            terminal_total_difficulty_passed: true,
            _non_exhaustive: (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_op_upgrades_to_el_forks() {
        let cfg = BaseSepolia::l1_config();

        assert_eq!(cfg.chain_id, u64::from(NamedChain::BaseSepolia));
        assert_eq!(cfg.shanghai_time, Some(BaseSepolia::CANYON_TIME));
        assert_eq!(cfg.cancun_time, Some(BaseSepolia::ECOTONE_TIME));
        assert_eq!(cfg.prague_time, Some(BaseSepolia::ISTHMUS_TIME));
        // Azul (the Osaka-equivalent OP upgrade) is not live, so Osaka is unscheduled.
        assert_eq!(cfg.osaka_time, None);
    }

    #[test]
    fn is_post_merge_from_genesis() {
        let cfg = BaseSepolia::l1_config();

        assert_eq!(cfg.london_block, Some(0));
        assert_eq!(cfg.merge_netsplit_block, Some(0));
        assert_eq!(cfg.terminal_total_difficulty, Some(U256::ZERO));
        assert!(cfg.terminal_total_difficulty_passed);
        assert!(cfg.ethash.is_none());
        assert!(cfg.deposit_contract_address.is_none());
    }
}
