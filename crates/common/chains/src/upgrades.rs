use alloy_hardforks::{EthereumHardforks, ForkCondition};
use alloy_primitives::Address;
use base_common_genesis::{BaseUpgrade, RollupConfig};

/// Extends [`EthereumHardforks`] with Base upgrade helper methods.
#[auto_impl::auto_impl(&, Arc)]
pub trait Upgrades: EthereumHardforks {
    /// Retrieves [`ForkCondition`] by a [`BaseUpgrade`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn fork_condition(&self, fork: BaseUpgrade) -> ForkCondition;

    /// Returns the activation registry admin address.
    fn activation_admin_address(&self) -> Option<Address> {
        None
    }

    /// Convenience method to check if [`BaseUpgrade::Bedrock`] is active at a given block
    /// number.
    fn is_bedrock_active_at_block(&self, block_number: u64) -> bool {
        self.fork_condition(BaseUpgrade::Bedrock).active_at_block(block_number)
    }

    /// Returns `true` if [`Regolith`](BaseUpgrade::Regolith) is active at given block
    /// timestamp.
    fn is_regolith_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Regolith).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Canyon`](BaseUpgrade::Canyon) is active at given block timestamp.
    fn is_canyon_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Canyon).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Ecotone`](BaseUpgrade::Ecotone) is active at given block timestamp.
    fn is_ecotone_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Ecotone).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Fjord`](BaseUpgrade::Fjord) is active at given block timestamp.
    fn is_fjord_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Fjord).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Granite`](BaseUpgrade::Granite) is active at given block timestamp.
    fn is_granite_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Granite).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Holocene`](BaseUpgrade::Holocene) is active at given block
    /// timestamp.
    fn is_holocene_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Holocene).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Isthmus`](BaseUpgrade::Isthmus) is active at given block
    /// timestamp.
    fn is_isthmus_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Isthmus).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Jovian`](BaseUpgrade::Jovian) is active at given block
    /// timestamp.
    fn is_jovian_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Jovian).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Azul`](BaseUpgrade::Azul) is active at given block timestamp.
    fn is_azul_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Azul).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Beryl`](BaseUpgrade::Beryl) is active at given block timestamp.
    fn is_beryl_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Beryl).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Cobalt`](BaseUpgrade::Cobalt) is active at given block timestamp.
    fn is_cobalt_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Cobalt).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`Denim`](BaseUpgrade::Denim) is active at given block timestamp.
    /// Denim is unscheduled by default, so this returns `false` until an activation time is
    /// configured via genesis or the L1 upgrade signal.
    fn is_denim_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Denim).active_at_timestamp(timestamp)
    }

    /// Returns `true` if the [`Zenith`](BaseUpgrade::Zenith) gate is active at the given block
    /// timestamp. Zenith is the permanently unscheduled gate for future hardfork feature
    /// testing: it is never contract-backed, so it can only be activated through genesis
    /// config (e.g. on a devnet); on real chains this returns `false`.
    fn is_zenith_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.fork_condition(BaseUpgrade::Zenith).active_at_timestamp(timestamp)
    }
}

impl Upgrades for RollupConfig {
    fn fork_condition(&self, fork: BaseUpgrade) -> ForkCondition {
        match fork {
            BaseUpgrade::Bedrock => ForkCondition::Block(0),
            BaseUpgrade::Regolith => self
                .upgrade_activation_timestamp(BaseUpgrade::Regolith)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Canyon)),
            BaseUpgrade::Canyon => self
                .upgrade_activation_timestamp(BaseUpgrade::Canyon)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Ecotone)),
            BaseUpgrade::Ecotone => self
                .upgrade_activation_timestamp(BaseUpgrade::Ecotone)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Fjord)),
            BaseUpgrade::Fjord => self
                .upgrade_activation_timestamp(BaseUpgrade::Fjord)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Granite)),
            BaseUpgrade::Granite => self
                .upgrade_activation_timestamp(BaseUpgrade::Granite)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Holocene)),
            BaseUpgrade::Holocene => self
                .upgrade_activation_timestamp(BaseUpgrade::Holocene)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Isthmus)),
            BaseUpgrade::Isthmus => self
                .upgrade_activation_timestamp(BaseUpgrade::Isthmus)
                .map(ForkCondition::Timestamp)
                .unwrap_or_else(|| self.fork_condition(BaseUpgrade::Jovian)),
            BaseUpgrade::Jovian => self
                .upgrade_activation_timestamp(BaseUpgrade::Jovian)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Azul => self
                .upgrade_activation_timestamp(BaseUpgrade::Azul)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Beryl => self
                .upgrade_activation_timestamp(BaseUpgrade::Beryl)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Cobalt => self
                .upgrade_activation_timestamp(BaseUpgrade::Cobalt)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            BaseUpgrade::Denim => self
                .upgrade_activation_timestamp(BaseUpgrade::Denim)
                .map(ForkCondition::Timestamp)
                .unwrap_or(ForkCondition::Never),
            // Zenith is the genesis-only gate for future hardfork feature testing: the runtime
            // registry drops Zenith writes, so only a genesis-configured timestamp can appear
            // here.
            BaseUpgrade::Zenith => self
                .upgrade_activation_timestamp(BaseUpgrade::Zenith)
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
        assert_eq!(cfg.fork_condition(BaseUpgrade::Regolith), ForkCondition::Timestamp(ACTIVATION));
        assert_eq!(cfg.fork_condition(BaseUpgrade::Canyon), ForkCondition::Timestamp(ACTIVATION));
        assert_eq!(cfg.fork_condition(BaseUpgrade::Ecotone), ForkCondition::Timestamp(ACTIVATION));

        // Bedrock is always at block 0; later forks unset are Never.
        assert_eq!(cfg.fork_condition(BaseUpgrade::Bedrock), ForkCondition::Block(0));
        assert_eq!(cfg.fork_condition(BaseUpgrade::Jovian), ForkCondition::Never);
        assert_eq!(cfg.fork_condition(BaseUpgrade::Azul), ForkCondition::Never);
        assert_eq!(cfg.fork_condition(BaseUpgrade::Beryl), ForkCondition::Never);
        assert_eq!(cfg.fork_condition(BaseUpgrade::Cobalt), ForkCondition::Never);
        assert_eq!(cfg.fork_condition(BaseUpgrade::Denim), ForkCondition::Never);
        assert_eq!(cfg.fork_condition(BaseUpgrade::Zenith), ForkCondition::Never);
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
        // Denim is contract-backed, so a runtime override can activate it on a live chain.
        RuntimeUpgradeRegistry::set_activation_timestamp(
            CHAIN_ID,
            BaseUpgrade::Denim,
            ACTIVATION + 2,
        );
        // Even a runtime override cannot activate the permanently-off Zenith gate.
        RuntimeUpgradeRegistry::set_activation_timestamp(CHAIN_ID, BaseUpgrade::Zenith, u64::MAX);

        assert_eq!(cfg.fork_condition(BaseUpgrade::Azul), ForkCondition::Timestamp(ACTIVATION));
        assert_eq!(
            cfg.fork_condition(BaseUpgrade::Cobalt),
            ForkCondition::Timestamp(ACTIVATION + 1)
        );
        assert_eq!(
            cfg.fork_condition(BaseUpgrade::Denim),
            ForkCondition::Timestamp(ACTIVATION + 2)
        );
        assert_eq!(cfg.fork_condition(BaseUpgrade::Zenith), ForkCondition::Never);

        RuntimeUpgradeRegistry::clear_chain(CHAIN_ID);
    }

    #[test]
    fn rollup_config_zenith_activates_via_genesis_config_only() {
        const ACTIVATION: u64 = 42;

        // Genesis config is the only way to schedule the Zenith testing gate.
        let mut cfg = RollupConfig::default();
        cfg.upgrades.base.zenith = Some(ACTIVATION);

        assert_eq!(cfg.fork_condition(BaseUpgrade::Zenith), ForkCondition::Timestamp(ACTIVATION));
        assert!(!cfg.is_zenith_active_at_timestamp(ACTIVATION - 1));
        assert!(cfg.is_zenith_active_at_timestamp(ACTIVATION));
    }
}
