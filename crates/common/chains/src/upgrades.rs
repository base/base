use alloy_hardforks::{EthereumHardforks, ForkCondition};
use base_common_genesis::RollupConfig;

use crate::BaseUpgrade;

/// Extends [`EthereumHardforks`] with Base upgrade helper methods.
#[auto_impl::auto_impl(&, Arc)]
pub trait Upgrades: EthereumHardforks {
    /// Retrieves [`ForkCondition`] by a [`BaseUpgrade`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition;

    /// Convenience method to check if [`BaseUpgrade::Bedrock`] is active at a given block
    /// number.
    fn is_bedrock_active_at_block(&self, block_number: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Bedrock).active_at_block(block_number)
    }

    /// Returns `true` if [`Regolith`](BaseUpgrade::Regolith) is active at given block
    /// timestamp.
    fn is_regolith_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Regolith).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Canyon`](BaseUpgrade::Canyon) is active at given block timestamp.
    fn is_canyon_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Canyon).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Ecotone`](BaseUpgrade::Ecotone) is active at given block timestamp.
    fn is_ecotone_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Ecotone).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Fjord`](BaseUpgrade::Fjord) is active at given block timestamp.
    fn is_fjord_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Fjord).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Granite`](BaseUpgrade::Granite) is active at given block timestamp.
    fn is_granite_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Granite).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Holocene`](BaseUpgrade::Holocene) is active at given block
    /// timestamp.
    fn is_holocene_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Holocene).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Isthmus`](BaseUpgrade::Isthmus) is active at given block
    /// timestamp.
    fn is_isthmus_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Isthmus).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Jovian`](BaseUpgrade::Jovian) is active at given block
    /// timestamp.
    fn is_jovian_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Jovian).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Azul`](BaseUpgrade::Azul) is active at given block timestamp.
    fn is_base_azul_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Azul).active_at_timestamp(timestamp)
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
