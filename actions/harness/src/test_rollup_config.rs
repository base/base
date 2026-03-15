use alloy_primitives::Address;
use base_consensus_genesis::{BaseHardforkConfig, HardForkConfig, RollupConfig};
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

    /// Replaces the entire hardfork schedule with the supplied [`HardForkConfig`].
    ///
    /// Use this when a test needs fine-grained control over which forks are active
    /// at which timestamps (e.g. hardfork boundary tests, span-batch gating tests).
    pub const fn with_hardforks(mut self, hardforks: HardForkConfig) -> Self {
        self.config.hardforks = hardforks;
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
        self.config.hardforks.base.get_or_insert_with(BaseHardforkConfig::default).v1 = Some(0);
        self
    }

    /// Finalizes the builder and returns the configured rollup config.
    pub const fn build(self) -> RollupConfig {
        self.config
    }
}
