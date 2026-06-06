//! Ethereum Sepolia testnet L1 chain configuration.

use alloy_chains::NamedChain;
use alloy_genesis::{ChainConfig, EthashConfig};
use alloy_primitives::{Address, U256, address};

/// Ethereum Sepolia testnet L1 chain configuration builder.
#[derive(Debug, Clone, Copy)]
pub struct Sepolia;

impl Sepolia {
    const TTD: u128 = 17_000_000_000_000_000u128;
    const DEPOSIT_CONTRACT_ADDRESS: Address =
        address!("0x7f02c3e3c98b133055b8b348b2ac625669ed295d");
    const MERGE_NETSPLIT_BLOCK: u64 = 1_735_371;

    /// Returns the Ethereum Sepolia testnet [`ChainConfig`].
    pub fn l1_config() -> ChainConfig {
        ChainConfig {
            chain_id: NamedChain::Sepolia.into(),
            homestead_block: alloy_hardforks::EthereumHardfork::Homestead
                .sepolia_activation_block(),
            dao_fork_block: alloy_hardforks::EthereumHardfork::Dao.sepolia_activation_block(),
            dao_fork_support: true,
            eip150_block: alloy_hardforks::EthereumHardfork::Tangerine.sepolia_activation_block(),
            eip155_block: alloy_hardforks::EthereumHardfork::SpuriousDragon
                .sepolia_activation_block(),
            eip158_block: alloy_hardforks::EthereumHardfork::SpuriousDragon
                .sepolia_activation_block(),
            byzantium_block: alloy_hardforks::EthereumHardfork::Byzantium
                .sepolia_activation_block(),
            constantinople_block: alloy_hardforks::EthereumHardfork::Constantinople
                .sepolia_activation_block(),
            petersburg_block: alloy_hardforks::EthereumHardfork::Petersburg
                .sepolia_activation_block(),
            istanbul_block: alloy_hardforks::EthereumHardfork::Istanbul.sepolia_activation_block(),
            muir_glacier_block: alloy_hardforks::EthereumHardfork::MuirGlacier
                .sepolia_activation_block(),
            berlin_block: alloy_hardforks::EthereumHardfork::Berlin.sepolia_activation_block(),
            london_block: alloy_hardforks::EthereumHardfork::London.sepolia_activation_block(),
            arrow_glacier_block: alloy_hardforks::EthereumHardfork::ArrowGlacier
                .sepolia_activation_block(),
            gray_glacier_block: alloy_hardforks::EthereumHardfork::GrayGlacier
                .sepolia_activation_block(),
            shanghai_time: alloy_hardforks::EthereumHardfork::Shanghai
                .sepolia_activation_timestamp(),
            cancun_time: alloy_hardforks::EthereumHardfork::Cancun.sepolia_activation_timestamp(),
            prague_time: alloy_hardforks::EthereumHardfork::Prague.sepolia_activation_timestamp(),
            osaka_time: alloy_hardforks::EthereumHardfork::Osaka.sepolia_activation_timestamp(),
            amsterdam_time: None,
            bpo1_time: alloy_hardforks::EthereumHardfork::Bpo1.sepolia_activation_timestamp(),
            bpo2_time: alloy_hardforks::EthereumHardfork::Bpo2.sepolia_activation_timestamp(),
            bpo3_time: alloy_hardforks::EthereumHardfork::Bpo3.sepolia_activation_timestamp(),
            bpo4_time: alloy_hardforks::EthereumHardfork::Bpo4.sepolia_activation_timestamp(),
            bpo5_time: alloy_hardforks::EthereumHardfork::Bpo5.sepolia_activation_timestamp(),
            ethash: Some(EthashConfig {}),
            blob_schedule: super::BlobSchedule::schedule(),
            terminal_total_difficulty: Some(U256::from(Self::TTD)),
            merge_netsplit_block: Some(Self::MERGE_NETSPLIT_BLOCK),
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
        sepolia::{SEPOLIA_BPO1_TIMESTAMP, SEPOLIA_BPO2_TIMESTAMP},
    };

    use super::*;

    #[test]
    fn test_bpo_timestamps() {
        let cfg = Sepolia::l1_config();

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
        assert_eq!(blob_schedule.scheduled[0].0, SEPOLIA_BPO1_TIMESTAMP);
        assert_eq!(blob_schedule.scheduled[1].0, SEPOLIA_BPO2_TIMESTAMP);
        assert_eq!(blob_schedule.scheduled[0].1, BlobParams::bpo1());
        assert_eq!(blob_schedule.scheduled[1].1, BlobParams::bpo2());
    }
}
