//! Chain parameters: which upgrades are active when, base fee parameters, and the Denim
//! timestamp schedule.

use core::num::NonZeroU64;

use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{Address, address};
use base_common_genesis::{
    BaseFeature, BaseUpgrade, DenimTimestampSchedule, FeeConfig, UpgradeConfig,
};

use crate::{BaseForkActivations, BaseSpecId};

/// The parameters of one Base chain that execution and validation depend on.
///
/// Built in for Base mainnet and Base Sepolia; other chains (devnets, the state fuzzer's
/// synthetic chain) come from a genesis file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSpec {
    /// The EIP-155 chain id.
    pub chain_id: u64,
    /// Activation timestamp of every upgrade after Bedrock. Bedrock is active from genesis.
    pub upgrades: UpgradeConfig,
    /// EIP-1559 parameters before and after Canyon.
    pub fee_config: FeeConfig,
    /// Number of the genesis block.
    pub genesis_number: u64,
    /// Timestamp of the genesis block.
    pub genesis_timestamp: u64,
    /// Seconds between blocks before Denim.
    pub block_time: u64,
    /// The activation registry admin Beryl seeds (before Cobalt stores it in state).
    pub activation_admin: Option<Address>,
}

impl ChainSpec {
    /// Base mainnet, chain 8453.
    pub const fn base_mainnet() -> Self {
        Self {
            chain_id: 8453,
            upgrades: UpgradeConfig::BASE_MAINNET,
            fee_config: FeeConfig {
                eip1559_elasticity: 6,
                eip1559_denominator: 50,
                eip1559_denominator_canyon: 250,
            },
            genesis_number: 0,
            genesis_timestamp: 1_686_789_347,
            block_time: 2,
            activation_admin: Some(address!("cE3a3bEE7E72E2A24079f3c0Cb3b97740ED425A9")),
        }
    }

    /// Base Sepolia, chain 84532.
    pub const fn base_sepolia() -> Self {
        Self {
            chain_id: 84532,
            upgrades: UpgradeConfig::BASE_SEPOLIA,
            fee_config: FeeConfig {
                eip1559_elasticity: 10,
                eip1559_denominator: 50,
                eip1559_denominator_canyon: 250,
            },
            genesis_number: 0,
            genesis_timestamp: 1_695_768_288,
            block_time: 2,
            activation_admin: Some(address!("5F43072722f59964d886CBb507F6a85ca0759D42")),
        }
    }

    /// Whether `upgrade` is active at `timestamp`. Bedrock is always active.
    pub fn is_upgrade_active(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool {
        upgrade == BaseUpgrade::Bedrock
            || self.upgrades.activation_timestamp(upgrade).is_some_and(|at| timestamp >= at)
    }

    /// Whether `feature` is active at `timestamp`: its upgrade's own activation time, as
    /// base-reth's chain spec decides it.
    pub fn is_active(&self, feature: BaseFeature, timestamp: u64) -> bool {
        self.is_upgrade_active(feature.activation_upgrade(), timestamp)
    }

    /// The latest execution upgrade active at `timestamp`, which selects the EVM spec.
    pub fn spec_at(&self, timestamp: u64) -> BaseSpecId {
        let upgrade = BaseUpgrade::EXECUTION_VARIANTS
            .into_iter()
            .rev()
            .find(|&upgrade| self.is_upgrade_active(upgrade, timestamp))
            .unwrap_or(BaseUpgrade::Bedrock);
        BaseSpecId::new(upgrade)
    }

    /// The chain's default EIP-1559 parameters at `timestamp`.
    pub fn base_fee_params(&self, timestamp: u64) -> BaseFeeParams {
        if self.is_active(BaseFeature::BaseFeeDenominator, timestamp) {
            self.fee_config.post_canyon_params()
        } else {
            self.fee_config.pre_canyon_params()
        }
    }

    /// The Denim timestamp schedule, when Denim is scheduled.
    pub fn denim_schedule(&self) -> Option<DenimTimestampSchedule> {
        let denim = self.upgrades.activation_timestamp(BaseUpgrade::Denim)?;
        let legacy_block_interval = NonZeroU64::new(self.block_time)?;
        Some(DenimTimestampSchedule {
            genesis_block_number: self.genesis_number,
            genesis_timestamp: self.genesis_timestamp,
            legacy_block_interval,
            denim_activation_timestamp: denim,
        })
    }
}

impl BaseForkActivations for ChainSpec {
    fn is_active_at_timestamp(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool {
        self.is_upgrade_active(upgrade, timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_spec_follows_the_schedule() {
        let spec = ChainSpec::base_mainnet();
        assert_eq!(spec.spec_at(1_686_789_347).upgrade(), BaseUpgrade::Regolith);
        assert_eq!(spec.spec_at(1_710_374_400).upgrade(), BaseUpgrade::Canyon);
        assert_eq!(spec.spec_at(1_710_374_401).upgrade(), BaseUpgrade::Ecotone);
        assert_eq!(spec.spec_at(1_793_901_600).upgrade(), BaseUpgrade::Denim);
        // Delta is not an execution upgrade: it never selects the spec.
        assert_eq!(spec.spec_at(1_708_560_000).upgrade(), BaseUpgrade::Canyon);
    }
}
