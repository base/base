use base_consensus_genesis::{BaseHardforkConfig, RollupConfig};
use base_consensus_registry::Registry;

use crate::BatcherConfig;

/// Builder for the mainnet-derived [`RollupConfig`] values used by harness tests.
#[derive(Debug, Clone)]
pub struct TestRollupConfigBuilder {
    config: RollupConfig,
}

impl TestRollupConfigBuilder {
    /// Starts from the Base mainnet config and applies the common harness overrides.
    ///
    /// This preserves the existing harness-test behavior by wiring the test batcher
    /// addresses, zeroing genesis for the in-memory L1 miner, and activating the
    /// Canyon-through-Fjord path from genesis.
    pub fn base_mainnet(batcher: &BatcherConfig) -> Self {
        let mut config = Registry::rollup_config(8453)
            .expect("Base mainnet config must exist in the registry")
            .clone();

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
        config.hardforks.canyon_time = Some(0);
        config.hardforks.delta_time = Some(0);
        config.hardforks.ecotone_time = Some(0);
        config.hardforks.fjord_time = Some(0);

        Self { config }
    }

    /// Overrides the channel timeout used before and after Granite activation.
    pub fn with_channel_timeout(mut self, n: u64) -> Self {
        self.config.channel_timeout = n;
        self.config.granite_channel_timeout = n;
        self
    }

    /// Overrides the pre-Fjord `max_sequencer_drift` field on the config.
    pub fn with_max_sequencer_drift(mut self, n: u64) -> Self {
        self.config.max_sequencer_drift = n;
        self
    }

    /// Activates every scheduled fork from genesis for tests that need it.
    ///
    /// `base_mainnet` intentionally keeps the harness's existing "Canyon through
    /// Fjord active" behavior; this opt-in extends that to the later upgrades.
    pub fn all_forks_active(mut self) -> Self {
        self.config.hardforks.regolith_time = Some(0);
        self.config.hardforks.canyon_time = Some(0);
        self.config.hardforks.delta_time = Some(0);
        self.config.hardforks.ecotone_time = Some(0);
        self.config.hardforks.fjord_time = Some(0);
        self.config.hardforks.granite_time = Some(0);
        self.config.hardforks.holocene_time = Some(0);
        self.config.hardforks.pectra_blob_schedule_time = Some(0);
        self.config.hardforks.isthmus_time = Some(0);
        self.config.hardforks.jovian_time = Some(0);
        self.config.hardforks.base.get_or_insert(BaseHardforkConfig::default()).v1 = Some(0);
        self
    }

    /// Finalizes the builder and returns the configured rollup config.
    pub fn build(self) -> RollupConfig {
        self.config
    }
}
