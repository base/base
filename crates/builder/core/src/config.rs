//! Builder Configuration

use core::time::Duration;
use std::sync::Arc;

use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
    config::{BaseBuilderConfig, BaseDAConfig, GasLimitConfig},
};

use crate::{
    BuilderMetrics, NoopMeteringProvider, RejectionCache, ResourceMeteringConfig,
    SharedMeteringProvider,
};

/// Configuration values for the Base block builder.
#[derive(Clone)]
pub struct BuilderConfig {
    /// Data Availability configuration for the payload builder.
    /// Defines constraints for the maximum size of data availability transactions.
    pub da_config: BaseDAConfig,

    /// Gas limit configuration for the payload builder
    pub gas_limit_config: GasLimitConfig,

    /// Hard cutoff on cumulative validity-predicate evaluation time per payload build.
    /// Once the cutoff is exceeded, further validity-gated transactions are deferred to a
    /// later build rather than evaluated. This is the guardrail backing the
    /// `base_builder_predicate_eval_duration_per_block` metric's P99 SLO.
    pub predicate_eval_hard_cutoff: Duration,

    /// Number of parked predicates that converts one state bucket to ordered wakeups.
    pub predicate_bucket_ordered_threshold: usize,

    /// Resource metering provider
    pub metering_provider: SharedMeteringProvider,
    /// Resource-unit metering used to throttle transactions in the native
    /// payload builder.
    pub resource_metering: ResourceMeteringConfig,

    /// Cache of permanently rejected transaction hashes, shared across blocks.
    /// Transactions in this cache are skipped by the iterator without re-evaluation.
    pub rejection_cache: RejectionCache,

    /// Whether to drop EIP-8130 transactions whose captured authorization
    /// predicates are positively stale before executing them.
    pub manifest_precheck_enabled: bool,

    /// Whether to record per-call state fetch latency for the build loop.
    ///
    /// Wraps the builder's state provider so account, storage, and code reads are timed and
    /// reported under `sync.state_provider` with a `builder` source label, separating build-loop
    /// IO from the engine's validation-path IO. Adds overhead to every state read, so this is
    /// driven by reth's `--engine.state-provider-metrics` and stays off by default.
    pub state_provider_metrics: bool,
}

impl BuilderConfig {
    /// Native payload builder settings derived from this configuration.
    ///
    /// The rejection cache and metering provider are shared by reference so the build loop and
    /// the `meterBundle` and `insertValidatedTransaction` RPC paths observe the same state.
    pub fn payload_builder_config(&self) -> BaseBuilderConfig {
        BaseBuilderConfig {
            da_config: self.da_config.clone(),
            gas_limit_config: self.gas_limit_config.clone(),
            manifest_precheck_enabled: self.manifest_precheck_enabled,
            predicate_eval_hard_cutoff: self.predicate_eval_hard_cutoff,
            predicate_bucket_ordered_threshold: self.predicate_bucket_ordered_threshold,
            resource_metering: self.resource_metering.clone(),
            rejection_cache: self.rejection_cache.clone(),
            state_provider_metrics: self.state_provider_metrics,
        }
    }

    /// Records startup resource-metering configuration metrics once the node's
    /// Prometheus recorder is installed.
    pub fn record_startup_metrics(&self) {
        if self.resource_metering.enabled && self.resource_metering.schedule.is_empty() {
            BuilderMetrics::resource_metering_schedule_missing_total().increment(1);
        }
    }
}

impl core::fmt::Debug for BuilderConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("da_config", &self.da_config)
            .field("gas_limit_config", &self.gas_limit_config)
            .field("predicate_eval_hard_cutoff", &self.predicate_eval_hard_cutoff)
            .field("predicate_bucket_ordered_threshold", &self.predicate_bucket_ordered_threshold)
            .field("metering_provider", &self.metering_provider)
            .field("resource_metering", &self.resource_metering)
            .field("rejection_cache_size", &self.rejection_cache.entry_count())
            .field("manifest_precheck_enabled", &self.manifest_precheck_enabled)
            .field("state_provider_metrics", &self.state_provider_metrics)
            .finish()
    }
}

impl Default for BuilderConfig {
    fn default() -> Self {
        let metering_provider: SharedMeteringProvider = Arc::new(NoopMeteringProvider);
        Self {
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
            predicate_eval_hard_cutoff: Duration::from_millis(10),
            predicate_bucket_ordered_threshold: DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
            metering_provider: Arc::clone(&metering_provider),
            resource_metering: ResourceMeteringConfig {
                provider: metering_provider,
                ..ResourceMeteringConfig::default()
            },
            rejection_cache: RejectionCache::default(),
            manifest_precheck_enabled: true,
            state_provider_metrics: false,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl BuilderConfig {
    /// Creates a new [`BuilderConfig`] suitable for testing.
    pub fn for_tests() -> Self {
        Self::default()
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

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;
    use base_execution_payload_builder::ResourceMeteringSchedule;

    use super::*;

    #[test]
    fn payload_builder_config_shares_metering_provider_and_rejection_cache() {
        let builder_config = BuilderConfig::default();
        let hash = TxHash::repeat_byte(0x11);
        builder_config.rejection_cache.insert(hash);
        let provider = Arc::clone(&builder_config.metering_provider);

        let native = builder_config.payload_builder_config();
        assert!(native.rejection_cache.contains_key(&hash));
        assert!(!native.resource_metering.enabled);
        assert!(native.resource_metering.schedule.is_empty());
        assert!(!native.resource_metering.is_active());
        assert!(Arc::ptr_eq(&native.resource_metering.provider, &provider));
    }

    #[test]
    fn payload_builder_config_propagates_resource_metering_schedule() {
        let schedule = ResourceMeteringSchedule::from_json(
            r#"{
                "version": 1,
                "dimensions": [{
                    "name": "compute",
                    "blockLimit": 1000,
                    "transactionLimit": 500,
                    "baseGasWeight": 1,
                    "operations": [{"name": "SSTORE", "gasUsedWeight": 2, "countCost": 10}],
                    "dryRun": true
                }]
            }"#,
        )
        .expect("valid schedule");

        let mut builder_config = BuilderConfig::default();
        let provider = Arc::clone(&builder_config.metering_provider);
        builder_config.resource_metering = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(schedule),
            provider: Arc::clone(&provider),
        };

        let native = builder_config.payload_builder_config();
        assert!(native.resource_metering.enabled);
        assert!(native.resource_metering.is_active());
        assert_eq!(native.resource_metering.schedule.dimensions.len(), 1);
        assert_eq!(native.resource_metering.schedule.dimensions[0].name, "compute");
        assert!(Arc::ptr_eq(&native.resource_metering.provider, &provider));
    }
}
