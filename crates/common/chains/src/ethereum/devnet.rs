//! Ethereum L1 configuration for the local Docker devnet.

use alloy_genesis::ChainConfig;
use alloy_primitives::U256;

/// Local Docker devnet L1 chain configuration builder.
#[derive(Debug, Clone, Copy)]
pub struct Devnet;

impl Devnet {
    /// Local Docker devnet L1 chain ID.
    pub const CHAIN_ID: u64 = 1337;

    /// Returns the local Docker devnet L1 [`ChainConfig`].
    pub fn l1_config() -> ChainConfig {
        ChainConfig {
            chain_id: Self::CHAIN_ID,
            homestead_block: Some(0),
            dao_fork_block: None,
            dao_fork_support: false,
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            muir_glacier_block: None,
            berlin_block: Some(0),
            london_block: Some(0),
            arrow_glacier_block: Some(0),
            gray_glacier_block: Some(0),
            shanghai_time: Some(0),
            cancun_time: Some(0),
            prague_time: Some(0),
            osaka_time: Some(0),
            amsterdam_time: None,
            bpo1_time: Some(0),
            bpo2_time: Some(0),
            bpo3_time: None,
            bpo4_time: None,
            bpo5_time: None,
            ethash: None,
            blob_schedule: super::BlobSchedule::schedule(),
            merge_netsplit_block: None,
            terminal_total_difficulty: Some(U256::ZERO),
            deposit_contract_address: None,
            clique: None,
            parlia: None,
            extra_fields: Default::default(),
            terminal_total_difficulty_passed: false,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_matches_docker_devnet_genesis() {
        let config = Devnet::l1_config();

        assert_eq!(config.chain_id, Devnet::CHAIN_ID);
        assert_eq!(config.shanghai_time, Some(0));
        assert_eq!(config.cancun_time, Some(0));
        assert_eq!(config.prague_time, Some(0));
        assert_eq!(config.osaka_time, Some(0));
        assert_eq!(config.bpo1_time, Some(0));
        assert_eq!(config.bpo2_time, Some(0));
        assert_eq!(config.blob_schedule, super::super::BlobSchedule::schedule());
    }
}
