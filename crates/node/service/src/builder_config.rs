//! Builder Configuration

use core::time::Duration;
use std::sync::Arc;

use base_execution_payload_builder::{
    MeteringStore, SharedMeteringStore,
    config::{BaseBuilderConfig, BaseDAConfig, GasLimitConfig},
};

use crate::BasePayloadServiceConfig;

/// Configuration values for the full-block builder.
#[derive(Clone)]
pub struct BuilderConfig {
    /// The interval at which blocks are added to the chain.
    /// This is also the frequency at which the builder will be receiving FCU requests from the
    /// sequencer.
    pub block_time: Duration,

    /// Data Availability configuration for the payload builder.
    /// Defines constraints for the maximum size of data availability transactions.
    pub da_config: BaseDAConfig,

    /// Gas limit configuration for the payload builder
    pub gas_limit_config: GasLimitConfig,

    /// Extra time allowed for payload building before garbage collection.
    pub block_time_leeway: Duration,

    /// Maximum gas a transaction can use before being excluded.
    pub max_gas_per_txn: Option<u64>,

    /// Maximum cumulative uncompressed (EIP-2718 encoded) block size in bytes.
    pub max_uncompressed_block_size: Option<u64>,

    /// Hard cutoff on cumulative validity-predicate evaluation time per builder iteration.
    /// Once the cutoff is exceeded, further validity-gated transactions are deferred to a
    /// later iteration rather than evaluated.
    pub predicate_eval_hard_cutoff: Duration,

    /// Resource metering provider
    pub metering_provider: SharedMeteringStore,

    /// Whether to drop EIP-8130 transactions whose captured authorization
    /// predicates are positively stale before executing them.
    pub manifest_precheck_enabled: bool,
}

impl core::fmt::Debug for BuilderConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("block_time", &self.block_time)
            .field("block_time_leeway", &self.block_time_leeway)
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("max_gas_per_txn", &self.max_gas_per_txn)
            .field("max_uncompressed_block_size", &self.max_uncompressed_block_size)
            .field("predicate_eval_hard_cutoff", &self.predicate_eval_hard_cutoff)
            .field("metering_provider", &self.metering_provider)
            .field("manifest_precheck_enabled", &self.manifest_precheck_enabled)
            .finish()
    }
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            block_time: Duration::from_secs(2),
            block_time_leeway: Duration::from_millis(500),
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
            max_gas_per_txn: None,
            max_uncompressed_block_size: None,
            predicate_eval_hard_cutoff: Duration::from_millis(10),
            metering_provider: Arc::new(MeteringStore::default()),
            manifest_precheck_enabled: true,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl BuilderConfig {
    /// Creates a new [`BuilderConfig`] suitable for testing.
    pub fn for_tests() -> Self {
        Self { block_time: Duration::from_secs(1), ..Self::default() }
    }

    /// Sets the block time in milliseconds.
    #[must_use]
    pub const fn with_block_time_ms(mut self, ms: u64) -> Self {
        self.block_time = Duration::from_millis(ms);
        self
    }

    /// Sets the maximum gas per transaction.
    #[must_use]
    pub const fn with_max_gas_per_txn(mut self, max_gas: Option<u64>) -> Self {
        self.max_gas_per_txn = max_gas;
        self
    }

    /// Sets the maximum uncompressed block size.
    #[must_use]
    pub const fn with_max_uncompressed_block_size(
        mut self,
        max_uncompressed_block_size: Option<u64>,
    ) -> Self {
        self.max_uncompressed_block_size = max_uncompressed_block_size;
        self
    }

    /// Toggles the EIP-8130 manifest precheck.
    #[must_use]
    pub const fn with_manifest_precheck_enabled(mut self, enabled: bool) -> Self {
        self.manifest_precheck_enabled = enabled;
        self
    }

    /// Sets the validity-predicate evaluation hard cutoff in milliseconds.
    #[must_use]
    pub const fn with_predicate_eval_hard_cutoff_ms(mut self, ms: u64) -> Self {
        self.predicate_eval_hard_cutoff = Duration::from_millis(ms);
        self
    }
}

impl BuilderConfig {
    /// Configures full-block payload construction and its deadline.
    pub fn into_payload_service_config(self) -> BasePayloadServiceConfig {
        BasePayloadServiceConfig::full_block(
            BaseBuilderConfig {
                da_config: self.da_config,
                gas_limit_config: self.gas_limit_config,
                manifest_precheck_enabled: self.manifest_precheck_enabled,
                predicate_eval_hard_cutoff: self.predicate_eval_hard_cutoff,
                max_gas_per_txn: self.max_gas_per_txn,
                max_uncompressed_block_size: self.max_uncompressed_block_size,
                ..Default::default()
            },
            self.block_time.saturating_add(self.block_time_leeway),
        )
    }
}
