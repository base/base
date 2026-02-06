//! Default rollup configuration matching Go's `DefaultDeployConfig()`.

use std::collections::BTreeMap;

use alloy_eips::eip1898::BlockNumHash;
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::Address;
use kona_genesis::{
    BaseFeeConfig, ChainGenesis, HardForkConfig, L1ChainConfig, RollupConfig, SystemConfig,
};

/// Create a default rollup config matching Go's `DefaultDeployConfig()`.
///
/// This provides the template configuration with all forks active at genesis.
/// Chain-specific values should be overwritten after calling this function.
pub fn default_rollup_config() -> RollupConfig {
    RollupConfig {
        // Will be overwritten by PerChainConfig
        l1_chain_id: 1,
        l2_chain_id: alloy_chains::Chain::from_id(1),

        // Genesis will be overwritten by PerChainConfig
        genesis: default_genesis(),

        // Timing parameters from Go's L2CoreDeployConfig
        block_time: 2,
        max_sequencer_drift: 600,
        seq_window_size: 3600,
        channel_timeout: 300,

        // Granite channel timeout (same as channel_timeout)
        granite_channel_timeout: 300,

        // Will be overwritten by PerChainConfig
        deposit_contract_address: Address::ZERO,
        l1_system_config_address: Address::ZERO,

        // Protocol addresses (defaults)
        batch_inbox_address: Address::ZERO,
        protocol_versions_address: Address::ZERO,
        da_challenge_address: None,
        superchain_config_address: None,

        // Blob configuration
        blobs_enabled_l1_timestamp: Some(0),

        // All forks active at genesis (time offset 0)
        hardforks: HardForkConfig {
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            holocene_time: Some(0),
            pectra_blob_schedule_time: None,
            isthmus_time: Some(0),
            jovian_time: None,
            interop_time: None,
        },

        // Interop message expiry window (default 0)
        interop_message_expiry_window: 0,

        // Alt DA configuration (disabled by default)
        alt_da_config: None,

        // Base fee config (Optimism defaults)
        chain_op_config: BaseFeeConfig::optimism(),
    }
}

/// Create a default L1 chain config for mainnet.
///
/// This provides a template L1 configuration with all relevant hardforks.
/// For Sepolia or other testnets, you should use chain-specific configurations.
#[must_use]
pub fn default_l1_config() -> L1ChainConfig {
    // Build the blob schedule with hardfork name -> BlobParams mapping
    let blob_schedule: BTreeMap<String, BlobParams> = BTreeMap::from([
        ("cancun".to_string(), BlobParams::cancun()),
        ("prague".to_string(), BlobParams::prague()),
        ("osaka".to_string(), BlobParams::osaka()),
        ("bpo1".to_string(), BlobParams::bpo1()),
        ("bpo2".to_string(), BlobParams::bpo2()),
    ]);

    // Mainnet L1 chain config with proper hardfork block numbers/timestamps
    L1ChainConfig {
        chain_id: 1,
        homestead_block: Some(1_150_000),
        eip150_block: Some(2_463_000),
        eip155_block: Some(2_675_000),
        eip158_block: Some(2_675_000),
        byzantium_block: Some(4_370_000),
        constantinople_block: Some(7_280_000),
        petersburg_block: Some(7_280_000),
        istanbul_block: Some(9_069_000),
        berlin_block: Some(12_244_000),
        london_block: Some(12_965_000),
        // Merge (Paris) happened at terminal total difficulty
        terminal_total_difficulty_passed: true,
        // Shanghai at 1681338455 (Apr 12, 2023)
        shanghai_time: Some(1_681_338_455),
        // Cancun at 1710338135 (Mar 13, 2024)
        cancun_time: Some(1_710_338_135),
        // Prague - not yet activated on mainnet, use a far future timestamp
        prague_time: None,
        // BPO hardforks not yet activated on mainnet
        bpo1_time: None,
        bpo2_time: None,
        // Blob schedule for correct blob base fee calculation
        blob_schedule,
        ..Default::default()
    }
}

/// Create default genesis configuration.
fn default_genesis() -> ChainGenesis {
    ChainGenesis {
        l1: BlockNumHash::default(),
        l2: BlockNumHash::default(),
        l2_time: 0,
        system_config: Some(SystemConfig {
            gas_limit: 30_000_000,
            ..SystemConfig::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_rollup_config_values() {
        let config = default_rollup_config();

        // Verify timing parameters from Go's L2CoreDeployConfig
        assert_eq!(config.block_time, 2);
        assert_eq!(config.max_sequencer_drift, 600);
        assert_eq!(config.seq_window_size, 3600);
        assert_eq!(config.channel_timeout, 300);
    }

    #[test]
    fn test_all_forks_active_at_genesis() {
        let config = default_rollup_config();

        // All forks should be active at genesis (time = 0)
        assert_eq!(config.hardforks.canyon_time, Some(0));
        assert_eq!(config.hardforks.delta_time, Some(0));
        assert_eq!(config.hardforks.ecotone_time, Some(0));
        assert_eq!(config.hardforks.fjord_time, Some(0));
        assert_eq!(config.hardforks.granite_time, Some(0));
        assert_eq!(config.hardforks.holocene_time, Some(0));
        assert_eq!(config.hardforks.isthmus_time, Some(0));

        // Regolith should also be active at genesis
        assert_eq!(config.hardforks.regolith_time, Some(0));
    }

    #[test]
    fn test_default_gas_limit() {
        let config = default_rollup_config();
        assert_eq!(config.genesis.system_config.unwrap().gas_limit, 30_000_000);
    }
}
