use alloy_hardforks::{EthereumHardforks, ForkCondition};
use alloy_primitives::Address;
use base_common_genesis::{BaseUpgrade, RollupConfig};

/// Extends [`EthereumHardforks`] with Base upgrade helper methods.
#[auto_impl::auto_impl(&, Arc)]
pub trait Upgrades: EthereumHardforks {
    /// Retrieves [`ForkCondition`] by a [`BaseUpgrade`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition;

    /// Returns the activation registry admin address.
    fn activation_admin_address(&self) -> Option<Address> {
        None
    }

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
    fn is_azul_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Azul).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Beryl`](BaseUpgrade::Beryl) is active at given block timestamp.
    fn is_beryl_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Beryl).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Cobalt`](BaseUpgrade::Cobalt) is active at given block timestamp.
    fn is_cobalt_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.upgrade_activation(BaseUpgrade::Cobalt).active_at_timestamp(timestamp)
    }
}

impl Upgrades for RollupConfig {
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
        match fork {
            BaseUpgrade::Bedrock => ForkCondition::Block(0),
            BaseUpgrade::Regolith => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Regolith)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Canyon)),
            BaseUpgrade::Canyon => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Canyon)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Ecotone)),
            BaseUpgrade::Ecotone => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Ecotone)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Fjord)),
            BaseUpgrade::Fjord => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Fjord)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Granite)),
            BaseUpgrade::Granite => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Granite)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Holocene)),
            BaseUpgrade::Holocene => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Holocene)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Isthmus)),
            BaseUpgrade::Isthmus => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Isthmus)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Jovian)),
            BaseUpgrade::Jovian => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Jovian)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Azul => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Azul)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Beryl => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Beryl)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Cobalt => self
                .contract_upgrade_activation_timestamp(BaseUpgrade::Cobalt)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            // Contract-only upgrades (Delta, PectraBlobSchedule) and any future variants are
            // absent from the execution fork ladder.
            _ => ForkCondition::Never,
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
        cfg.upgrades.ecotone_time = Some(ACTIVATION);

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
        assert_eq!(cfg.upgrade_activation(BaseUpgrade::Beryl), ForkCondition::Never);
        assert_eq!(cfg.upgrade_activation(BaseUpgrade::Cobalt), ForkCondition::Never);
    }

    #[cfg(feature = "std")]
    #[test]
    fn rollup_config_upgrade_activation_uses_runtime_overrides() {
        use base_common_genesis::RuntimeUpgradeRegistry;

        const CHAIN_ID: u64 = 9_777_001;
        const ACTIVATION: u64 = 42;

        let cfg = RollupConfig {
            l2_chain_id: alloy_chains::Chain::from_id(CHAIN_ID),
            ..RollupConfig::default()
        };
        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
        RuntimeUpgradeRegistry::set_activation_timestamp(CHAIN_ID, BaseUpgrade::Azul, ACTIVATION);
        RuntimeUpgradeRegistry::set_activation_timestamp(
            CHAIN_ID,
            BaseUpgrade::Cobalt,
            ACTIVATION + 1,
        );

        assert_eq!(cfg.upgrade_activation(BaseUpgrade::Azul), ForkCondition::Timestamp(ACTIVATION));
        assert_eq!(
            cfg.upgrade_activation(BaseUpgrade::Cobalt),
            ForkCondition::Timestamp(ACTIVATION + 1)
        );

        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
    }
}
