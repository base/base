//! Ethereum mainnet L1 chain configuration.

use alloy_chains::NamedChain;
use alloy_genesis::{ChainConfig, EthashConfig};
use alloy_primitives::{Address, U256, address};

/// Ethereum mainnet L1 chain configuration builder.
#[derive(Debug, Clone, Copy)]
pub struct Mainnet;

impl Mainnet {
    const TTD: u128 = 58_750_000_000_000_000_000_000u128;
    const DEPOSIT_CONTRACT_ADDRESS: Address =
        address!("0x00000000219ab540356cbb839cbe05303d7705fa");

    /// Returns the Ethereum mainnet [`ChainConfig`].
    pub fn l1_config() -> ChainConfig {
        ChainConfig {
            chain_id: NamedChain::Mainnet.into(),
            homestead_block: alloy_hardforks::EthereumHardfork::Homestead
                .mainnet_activation_block(),
            dao_fork_block: alloy_hardforks::EthereumHardfork::Dao.mainnet_activation_block(),
            dao_fork_support: true,
            eip150_block: alloy_hardforks::EthereumHardfork::Tangerine.mainnet_activation_block(),
            eip155_block: alloy_hardforks::EthereumHardfork::SpuriousDragon
                .mainnet_activation_block(),
            eip158_block: alloy_hardforks::EthereumHardfork::SpuriousDragon
                .mainnet_activation_block(),
            byzantium_block: alloy_hardforks::EthereumHardfork::Byzantium
                .mainnet_activation_block(),
            constantinople_block: alloy_hardforks::EthereumHardfork::Constantinople
                .mainnet_activation_block(),
            petersburg_block: alloy_hardforks::EthereumHardfork::Petersburg
                .mainnet_activation_block(),
            istanbul_block: alloy_hardforks::EthereumHardfork::Istanbul.mainnet_activation_block(),
            muir_glacier_block: alloy_hardforks::EthereumHardfork::MuirGlacier
                .mainnet_activation_block(),
            berlin_block: alloy_hardforks::EthereumHardfork::Berlin.mainnet_activation_block(),
            london_block: alloy_hardforks::EthereumHardfork::London.mainnet_activation_block(),
            arrow_glacier_block: alloy_hardforks::EthereumHardfork::ArrowGlacier
                .mainnet_activation_block(),
            gray_glacier_block: alloy_hardforks::EthereumHardfork::GrayGlacier
                .mainnet_activation_block(),
            shanghai_time: alloy_hardforks::EthereumHardfork::Shanghai
                .mainnet_activation_timestamp(),
            cancun_time: alloy_hardforks::EthereumHardfork::Cancun.mainnet_activation_timestamp(),
            prague_time: alloy_hardforks::EthereumHardfork::Prague.mainnet_activation_timestamp(),
            osaka_time: alloy_hardforks::EthereumHardfork::Osaka.mainnet_activation_timestamp(),
            amsterdam_time: None,
            bpo1_time: alloy_hardforks::EthereumHardfork::Bpo1.mainnet_activation_timestamp(),
            bpo2_time: alloy_hardforks::EthereumHardfork::Bpo2.mainnet_activation_timestamp(),
            bpo3_time: alloy_hardforks::EthereumHardfork::Bpo3.mainnet_activation_timestamp(),
            bpo4_time: alloy_hardforks::EthereumHardfork::Bpo4.mainnet_activation_timestamp(),
            bpo5_time: alloy_hardforks::EthereumHardfork::Bpo5.mainnet_activation_timestamp(),
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
    use alloy_hardforks::EthereumHardfork;

    use super::*;

    #[test]
    fn test_bpo_timestamps() {
        const BPO1_TIMESTAMP: u64 = 1_765_290_071;
        const BPO2_TIMESTAMP: u64 = 1_767_747_671;

        let cfg = Mainnet::l1_config();

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
        assert_eq!(blob_schedule.scheduled[0].0, BPO1_TIMESTAMP);
        assert_eq!(blob_schedule.scheduled[1].0, BPO2_TIMESTAMP);
        assert_eq!(blob_schedule.scheduled[0].1, BlobParams::bpo1());
        assert_eq!(blob_schedule.scheduled[1].1, BlobParams::bpo2());
    }
}
