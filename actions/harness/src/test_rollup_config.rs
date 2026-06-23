use alloy_primitives::Address;
use base_common_chains::{ChainConfig, rollup_config};
use base_common_genesis::{RollupConfig, UpgradeConfig};

use crate::BatcherConfig;

/// Builder for the mainnet-derived [`RollupConfig`] values used by harness tests.
#[derive(Debug, Clone)]
pub struct TestRollupConfigBuilder {
    config: RollupConfig,
}

impl TestRollupConfigBuilder {
    /// Returns the Base mainnet [`RollupConfig`] from [`ChainConfig::MAINNET`].
    pub fn mainnet() -> RollupConfig {
        rollup_config!(ChainConfig::MAINNET)
    }

    /// Starts from the Base mainnet config and applies the common harness overrides.
    ///
    /// This preserves the existing harness-test behavior by wiring the test batcher
    /// addresses, zeroing genesis for the in-memory L1 miner, and activating the
    /// Canyon-through-Fjord path from genesis.
    pub fn base_mainnet(batcher: &BatcherConfig) -> Self {
        let mut config = rollup_config!(ChainConfig::MAINNET);

        config.batch_inbox_address = batcher.inbox_address;
        config
            .genesis
            .system_config
            .as_mut()
            .expect("Base mainnet config must define a system config")
            .batcher_address = batcher.batcher_address;
        config.genesis.l2_time = 0;
        config.genesis.l1 = Default::default();
        config.genesis.l2 = Default::default();
        config.upgrades.canyon_time = Some(0);
        config.upgrades.delta_time = Some(0);
        config.upgrades.ecotone_time = Some(0);
        config.upgrades.fjord_time = Some(0);

        Self { config }
    }

    /// Overrides the channel timeout used before and after Granite activation.
    pub const fn with_channel_timeout(mut self, n: u64) -> Self {
        self.config.channel_timeout = n;
        self.config.granite_channel_timeout = n;
        self
    }

    /// Overrides the pre-Fjord `max_sequencer_drift` field on the config.
    pub const fn with_max_sequencer_drift(mut self, n: u64) -> Self {
        self.config.max_sequencer_drift = n;
        self
    }

    /// Overrides the L2 block time in seconds.
    pub const fn with_block_time(mut self, n: u64) -> Self {
        self.config.block_time = n;
        self
    }

    /// Overrides the sequencer window size (in L1 blocks).
    pub const fn with_seq_window_size(mut self, n: u64) -> Self {
        self.config.seq_window_size = n;
        self
    }

    /// Overrides the L1 system config contract address.
    pub const fn with_l1_system_config_address(mut self, addr: Address) -> Self {
        self.config.l1_system_config_address = addr;
        self
    }

    /// Overrides the L1 deposit contract address.
    pub const fn with_deposit_contract(mut self, addr: Address) -> Self {
        self.config.deposit_contract_address = addr;
        self
    }

    /// Replaces the entire upgrade schedule with the supplied [`UpgradeConfig`].
    ///
    /// Use this when a test needs fine-grained control over which forks are active
    /// at which timestamps (e.g. upgrade boundary tests, span-batch gating tests).
    pub const fn with_upgrades(mut self, upgrades: UpgradeConfig) -> Self {
        self.config.upgrades = upgrades;
        self
    }

    /// Activates all forks from Canyon through Granite at genesis, leaving Holocene
    /// and later as `None`.
    ///
    /// Replaces the entire upgrade schedule. Use when a test needs the last
    /// pre-Holocene cumulative schedule.
    pub fn through_granite(mut self) -> Self {
        self.config.upgrades = UpgradeConfig {
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            ..Default::default()
        };
        self
    }

    /// Activates all forks from Canyon through Holocene at genesis, leaving Isthmus
    /// and later as `None`.
    ///
    /// Replaces the entire upgrade schedule. Use when a test needs a cumulative
    /// schedule up to Holocene with no later forks reachable.
    pub fn through_holocene(mut self) -> Self {
        self.config.upgrades = UpgradeConfig {
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            holocene_time: Some(0),
            ..Default::default()
        };
        self
    }

    /// Activates all forks from Canyon through Isthmus at genesis, leaving Jovian
    /// and later as `None`.
    ///
    /// Replaces the entire upgrade schedule. Use when a test needs Isthmus active
    /// from genesis with no later forks reachable.
    pub fn through_isthmus(self) -> Self {
        let mut this = self.through_holocene();
        this.config.upgrades.isthmus_time = Some(0);
        this
    }

    /// Sets the Isthmus activation timestamp.
    ///
    /// Typically chained after [`through_holocene`](Self::through_holocene) to
    /// schedule Isthmus at a specific future timestamp.
    pub const fn with_isthmus_at(mut self, t: u64) -> Self {
        self.config.upgrades.isthmus_time = Some(t);
        self
    }

    /// Sets the Jovian activation timestamp.
    ///
    /// Typically chained after [`through_isthmus`](Self::through_isthmus) to
    /// schedule Jovian at a specific future timestamp.
    pub const fn with_jovian_at(mut self, t: u64) -> Self {
        self.config.upgrades.jovian_time = Some(t);
        self
    }

    /// Sets the Base Azul activation timestamp.
    ///
    /// Base Azul is a standalone Base-specific fork, independent of the
    /// inherited fork cascade. Chaining after any `through_*` method is fine.
    pub const fn with_azul_at(mut self, t: u64) -> Self {
        self.config.upgrades.base.azul = Some(t);
        self
    }

    /// Sets the Beryl activation timestamp.
    ///
    /// Beryl is a standalone Base-specific fork, independent of the inherited fork cascade.
    pub const fn with_beryl_at(mut self, t: u64) -> Self {
        self.config.upgrades.base.beryl = Some(t);
        self
    }

    /// Sets the Cobalt activation timestamp.
    ///
    /// Cobalt is a standalone Base-specific fork, independent of the inherited fork cascade.
    pub const fn with_cobalt_at(mut self, t: u64) -> Self {
        self.config.upgrades.base.cobalt = Some(t);
        self
    }

    /// Activates every scheduled fork from genesis for tests that need it.
    ///
    /// `base_mainnet` intentionally keeps the harness's existing "Canyon through
    /// Fjord active" behavior; this opt-in extends that to the later upgrades.
    pub const fn all_forks_active(mut self) -> Self {
        self.config.upgrades.regolith_time = Some(0);
        self.config.upgrades.canyon_time = Some(0);
        self.config.upgrades.delta_time = Some(0);
        self.config.upgrades.ecotone_time = Some(0);
        self.config.upgrades.fjord_time = Some(0);
        self.config.upgrades.granite_time = Some(0);
        self.config.upgrades.holocene_time = Some(0);
        self.config.upgrades.pectra_blob_schedule_time = Some(0);
        self.config.upgrades.isthmus_time = Some(0);
        self.config.upgrades.jovian_time = Some(0);
        self.config.upgrades.base.azul = Some(0);
        self.config.upgrades.base.beryl = Some(0);
        self
    }

    /// Finalizes the builder and returns the configured rollup config.
    pub const fn build(self) -> RollupConfig {
        self.config
    }
}
