//! Ethereum Hoodi testnet L1 chain configuration.

use alloy_chains::NamedChain;
use alloy_genesis::{ChainConfig, EthashConfig};
use alloy_primitives::{Address, U256, address};

/// Ethereum Hoodi testnet L1 chain configuration builder.
#[derive(Debug, Clone, Copy)]
pub struct Hoodi;

impl Hoodi {
    const TTD: u128 = 0;
    const DEPOSIT_CONTRACT_ADDRESS: Address =
        address!("0x4242424242424242424242424242424242424242");

    /// Returns the Ethereum Hoodi testnet [`ChainConfig`].
    pub fn l1_config() -> ChainConfig {
        ChainConfig {
            chain_id: NamedChain::Hoodi.into(),
            homestead_block: Some(0),
            dao_fork_block: Some(0),
            dao_fork_support: true,
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
            shanghai_time: alloy_hardforks::EthereumHardfork::Shanghai.hoodi_activation_timestamp(),
            cancun_time: alloy_hardforks::EthereumHardfork::Cancun.hoodi_activation_timestamp(),
            prague_time: alloy_hardforks::EthereumHardfork::Prague.hoodi_activation_timestamp(),
            osaka_time: alloy_hardforks::EthereumHardfork::Osaka.hoodi_activation_timestamp(),
            amsterdam_time: None,
            bpo1_time: alloy_hardforks::EthereumHardfork::Bpo1.hoodi_activation_timestamp(),
            bpo2_time: alloy_hardforks::EthereumHardfork::Bpo2.hoodi_activation_timestamp(),
            bpo3_time: alloy_hardforks::EthereumHardfork::Bpo3.hoodi_activation_timestamp(),
            bpo4_time: alloy_hardforks::EthereumHardfork::Bpo4.hoodi_activation_timestamp(),
            bpo5_time: alloy_hardforks::EthereumHardfork::Bpo5.hoodi_activation_timestamp(),
            ethash: Some(EthashConfig {}),
            blob_schedule: super::BlobSchedule::schedule(),
            merge_netsplit_block: None,
            terminal_total_difficulty: Some(U256::from(Self::TTD)),
            deposit_contract_address: Some(Self::DEPOSIT_CONTRACT_ADDRESS),
            clique: None,
            parlia: None,
            extra_fields: Default::default(),
            terminal_total_difficulty_passed: false,
            _non_exhaustive: (),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip7840::BlobParams;
    use alloy_hardforks::{
        EthereumHardfork,
        hoodi::{HOODI_BPO1_TIMESTAMP, HOODI_BPO2_TIMESTAMP},
    };

    use super::*;

    #[test]
    fn test_bpo_timestamps() {
        let cfg = Hoodi::l1_config();

        assert_eq!(cfg.blob_schedule.len(), 5);
        assert_eq!(
            cfg.blob_schedule.get(&EthereumHardfork::Bpo1.name().to_lowercase()).unwrap(),
            &BlobParams::bpo1()
        );
        assert_eq!(
            cfg.blob_schedule.get(&EthereumHardfork::Bpo2.name().to_lowercase()).unwrap(),
            &BlobParams::bpo2()
        );

        let blob_schedule = cfg.blob_schedule_blob_params();
        assert_eq!(blob_schedule.scheduled.len(), 2);
        assert_eq!(blob_schedule.scheduled[0].0, HOODI_BPO1_TIMESTAMP);
        assert_eq!(blob_schedule.scheduled[1].0, HOODI_BPO2_TIMESTAMP);
        assert_eq!(blob_schedule.scheduled[0].1, BlobParams::bpo1());
        assert_eq!(blob_schedule.scheduled[1].1, BlobParams::bpo2());
    }
}
