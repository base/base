//! Conversions from [`BaseChainConfig`] to downstream configuration types.

use alloy_chains::Chain;
use alloy_eips::BlockNumHash;
use base_alloy_chains::BaseChainConfig;

use crate::{
    BaseFeeConfig, BaseHardforkConfig, ChainGenesis, GRANITE_CHANNEL_TIMEOUT, HardForkConfig,
    RollupConfig, SystemConfig,
};

impl From<&BaseChainConfig> for HardForkConfig {
    fn from(cfg: &BaseChainConfig) -> Self {
        Self {
            regolith_time: Some(cfg.regolith_timestamp),
            canyon_time: Some(cfg.canyon_timestamp),
            delta_time: Some(cfg.delta_timestamp),
            ecotone_time: Some(cfg.ecotone_timestamp),
            fjord_time: Some(cfg.fjord_timestamp),
            granite_time: Some(cfg.granite_timestamp),
            holocene_time: Some(cfg.holocene_timestamp),
            pectra_blob_schedule_time: cfg.pectra_blob_schedule_timestamp,
            isthmus_time: Some(cfg.isthmus_timestamp),
            jovian_time: Some(cfg.jovian_timestamp),
            base: BaseHardforkConfig { v1: cfg.base_v1_timestamp },
        }
    }
}

impl From<&BaseChainConfig> for BaseFeeConfig {
    fn from(cfg: &BaseChainConfig) -> Self {
        Self {
            eip1559_elasticity: cfg.eip1559_elasticity,
            eip1559_denominator: cfg.eip1559_denominator,
            eip1559_denominator_canyon: cfg.eip1559_denominator_canyon,
        }
    }
}

impl From<&BaseChainConfig> for ChainGenesis {
    fn from(cfg: &BaseChainConfig) -> Self {
        Self {
            l1: BlockNumHash { hash: cfg.genesis_l1_hash, number: cfg.genesis_l1_number },
            l2: BlockNumHash { hash: cfg.genesis_l2_hash, number: cfg.genesis_l2_number },
            l2_time: cfg.genesis_l2_time,
            system_config: Some(SystemConfig {
                batcher_address: cfg.genesis_batcher_address,
                overhead: cfg.genesis_overhead,
                scalar: cfg.genesis_scalar,
                gas_limit: cfg.genesis_gas_limit,
                base_fee_scalar: None,
                blob_base_fee_scalar: None,
                eip1559_denominator: None,
                eip1559_elasticity: None,
                operator_fee_scalar: None,
                operator_fee_constant: None,
                min_base_fee: None,
                da_footprint_gas_scalar: None,
            }),
        }
    }
}

impl From<&BaseChainConfig> for RollupConfig {
    fn from(cfg: &BaseChainConfig) -> Self {
        Self {
            genesis: ChainGenesis::from(cfg),
            block_time: cfg.block_time,
            max_sequencer_drift: cfg.max_sequencer_drift,
            seq_window_size: cfg.seq_window_size,
            channel_timeout: cfg.channel_timeout,
            granite_channel_timeout: GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: cfg.l1_chain_id,
            l2_chain_id: Chain::from_id(cfg.chain_id),
            hardforks: HardForkConfig::from(cfg),
            batch_inbox_address: cfg.batch_inbox_address,
            deposit_contract_address: cfg.deposit_contract_address,
            l1_system_config_address: cfg.system_config_address,
            protocol_versions_address: cfg.protocol_versions_address,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: BaseFeeConfig::from(cfg),
        }
    }
}
