//! Derived views of [`ChainConfig`].
//!
//! Conversions and accessors that turn a static [`ChainConfig`] into the
//! genesis-layer types it parameterises ([`RollupConfig`], [`FeeConfig`],
//! [`ChainGenesis`], [`HardForkConfig`]).

use alloy_chains::Chain;
use alloy_eips::eip1898::BlockNumHash;
use alloy_hardforks::ForkCondition;
use base_consensus_genesis::{
    ChainGenesis, FeeConfig, HardForkConfig, HardforkConfig, RollupConfig, SystemConfig,
};

use crate::{BaseUpgrade, ChainConfig, Upgrades};

impl ChainConfig {
    /// Returns the EIP-1559 [`FeeConfig`] for this chain.
    pub const fn fee_config(&self) -> FeeConfig {
        FeeConfig {
            eip1559_elasticity: self.eip1559_elasticity,
            eip1559_denominator: self.eip1559_denominator,
            eip1559_denominator_canyon: self.eip1559_denominator_canyon,
        }
    }

    /// Returns the [`HardForkConfig`] (Base upgrade activation timestamps) for this chain.
    pub const fn hardfork_config(&self) -> HardForkConfig {
        HardForkConfig {
            regolith_time: Some(self.regolith_timestamp),
            canyon_time: Some(self.canyon_timestamp),
            delta_time: Some(self.delta_timestamp),
            ecotone_time: Some(self.ecotone_timestamp),
            fjord_time: Some(self.fjord_timestamp),
            granite_time: Some(self.granite_timestamp),
            holocene_time: Some(self.holocene_timestamp),
            pectra_blob_schedule_time: self.pectra_blob_schedule_timestamp,
            isthmus_time: Some(self.isthmus_timestamp),
            jovian_time: Some(self.jovian_timestamp),
            base: HardforkConfig { azul: self.azul_timestamp },
        }
    }

    /// Returns the [`ChainGenesis`] (L1/L2 genesis anchor + initial system config) for this chain.
    pub const fn chain_genesis(&self) -> ChainGenesis {
        ChainGenesis {
            l1: BlockNumHash { hash: self.genesis_l1_hash, number: self.genesis_l1_number },
            l2: BlockNumHash { hash: self.genesis_l2_hash, number: self.genesis_l2_number },
            l2_time: self.genesis_l2_time,
            system_config: Some(SystemConfig {
                batcher_address: self.genesis_batcher_address,
                overhead: self.genesis_overhead,
                scalar: self.genesis_scalar,
                gas_limit: self.genesis_gas_limit,
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

    /// Returns the full [`RollupConfig`] for this chain, derived from its [`ChainConfig`].
    pub fn rollup_config(&self) -> RollupConfig {
        RollupConfig {
            genesis: self.chain_genesis(),
            block_time: self.block_time,
            max_sequencer_drift: self.max_sequencer_drift,
            seq_window_size: self.seq_window_size,
            channel_timeout: self.channel_timeout,
            granite_channel_timeout: RollupConfig::GRANITE_CHANNEL_TIMEOUT,
            l1_chain_id: self.l1_chain_id,
            l2_chain_id: Chain::from_id(self.chain_id),
            hardforks: self.hardfork_config(),
            batch_inbox_address: self.batch_inbox_address,
            deposit_contract_address: self.deposit_contract_address,
            l1_system_config_address: self.system_config_address,
            protocol_versions_address: self.protocol_versions_address,
            blobs_enabled_l1_timestamp: None,
            chain_op_config: self.fee_config(),
        }
    }
}

impl From<&ChainConfig> for FeeConfig {
    fn from(cfg: &ChainConfig) -> Self {
        cfg.fee_config()
    }
}

impl From<&ChainConfig> for HardForkConfig {
    fn from(cfg: &ChainConfig) -> Self {
        cfg.hardfork_config()
    }
}

impl From<&ChainConfig> for ChainGenesis {
    fn from(cfg: &ChainConfig) -> Self {
        cfg.chain_genesis()
    }
}

impl From<&ChainConfig> for RollupConfig {
    fn from(cfg: &ChainConfig) -> Self {
        cfg.rollup_config()
    }
}

impl Upgrades for RollupConfig {
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
        match fork {
            BaseUpgrade::Bedrock => ForkCondition::Block(0),
            BaseUpgrade::Regolith => self
                .hardforks
                .regolith_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Canyon)),
            BaseUpgrade::Canyon => self
                .hardforks
                .canyon_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Ecotone)),
            BaseUpgrade::Ecotone => self
                .hardforks
                .ecotone_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Fjord)),
            BaseUpgrade::Fjord => self
                .hardforks
                .fjord_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Granite)),
            BaseUpgrade::Granite => self
                .hardforks
                .granite_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Holocene)),
            BaseUpgrade::Holocene => self
                .hardforks
                .holocene_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Isthmus)),
            BaseUpgrade::Isthmus => self
                .hardforks
                .isthmus_time
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Jovian)),
            BaseUpgrade::Jovian => self
                .hardforks
                .jovian_time
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            // Azul is standalone: not part of the Base upgrade cascade chain. It only activates
            // when explicitly configured and never implies (or is implied by) Jovian being active.
            BaseUpgrade::Azul => self
                .hardforks
                .base
                .azul
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_fee_config_matches_const() {
        // Guard against drift between the hardcoded `FeeConfig::BASE_MAINNET` constant
        // (used as a serde default) and the canonical `ChainConfig::mainnet().fee_config()`.
        assert_eq!(ChainConfig::mainnet().fee_config(), FeeConfig::base_mainnet());
    }

    #[test]
    fn rollup_config_upgrade_activation_cascade() {
        const ACTIVATION: u64 = 10;
        let mut cfg = RollupConfig::default();
        cfg.hardforks.ecotone_time = Some(ACTIVATION);

        // Cascading: Regolith and Canyon should fall through to Ecotone.
        assert_eq!(
            cfg.upgrade_activation(BaseUpgrade::Regolith),
            ForkCondition::Timestamp(ACTIVATION)
        );
        assert_eq!(
            cfg.upgrade_activation(BaseUpgrade::Canyon),
            ForkCondition::Timestamp(ACTIVATION)
        );
        assert_eq!(
            cfg.upgrade_activation(BaseUpgrade::Ecotone),
            ForkCondition::Timestamp(ACTIVATION)
        );

        // Bedrock is always at block 0; later forks unset are Never.
        assert_eq!(cfg.upgrade_activation(BaseUpgrade::Bedrock), ForkCondition::Block(0));
        assert_eq!(cfg.upgrade_activation(BaseUpgrade::Jovian), ForkCondition::Never);
        assert_eq!(cfg.upgrade_activation(BaseUpgrade::Azul), ForkCondition::Never);
    }
}
