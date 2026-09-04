//! Additional configuration for the Base payload builder.

use std::{
    path::Path,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use alloy_primitives::TxHash;
use revm::state::EvmState;
use tracing::{debug, warn};

use crate::{
    MeteringProvider, NoopMeteringProvider, RejectionCache, ResourceMeteringError,
    ResourceMeteringMetrics, ResourceMeteringSchedule, ResourceMeteringUsage, ResourceSample,
    ResourceThrottlingDecision, SharedMeteringProvider,
};

/// Settings for the Base payload builder.
#[derive(Debug, Clone)]
pub struct BaseBuilderConfig {
    /// Data availability configuration for the Base payload builder.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the Base payload builder.
    pub gas_limit_config: GasLimitConfig,
    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution.
    pub manifest_precheck_enabled: bool,
    /// Hard cutoff on cumulative validity-predicate evaluation time per payload build.
    pub predicate_eval_hard_cutoff: Duration,
    /// Resource metering and throttling configuration for payload admission.
    pub resource_metering: ResourceMeteringConfig,
    /// Shared, cross-job cache of permanently rejected transaction hashes.
    ///
    /// Native payload jobs skip hashes already in this cache even if the
    /// transaction is re-gossiped into the pool. Nonce-lane descendants are
    /// skipped for the current scan via `PayloadTransactions::mark_invalid`;
    /// skipping those descendants across later jobs is Flashblocks-only.
    pub rejection_cache: RejectionCache,
}

impl Default for BaseBuilderConfig {
    fn default() -> Self {
        Self {
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
            manifest_precheck_enabled: true,
            predicate_eval_hard_cutoff: Duration::from_millis(10),
            resource_metering: ResourceMeteringConfig::default(),
            rejection_cache: RejectionCache::default(),
        }
    }
}

impl BaseBuilderConfig {
    /// Creates a new Base payload builder configuration.
    pub fn new(
        da_config: BaseDAConfig,
        gas_limit_config: GasLimitConfig,
        manifest_precheck_enabled: bool,
    ) -> Self {
        Self {
            da_config,
            gas_limit_config,
            manifest_precheck_enabled,
            predicate_eval_hard_cutoff: Duration::from_millis(10),
            resource_metering: ResourceMeteringConfig::default(),
            rejection_cache: RejectionCache::default(),
        }
    }

    /// Sets resource metering and throttling for payload admission.
    pub fn with_resource_metering(mut self, resource_metering: ResourceMeteringConfig) -> Self {
        self.resource_metering = resource_metering;
        self
    }

    /// Returns the data availability configuration for the Base payload builder, if it has
    /// configured
    /// constraints.
    pub fn constrained_da_config(&self) -> Option<&BaseDAConfig> {
        if self.da_config.is_empty() { None } else { Some(&self.da_config) }
    }
}

/// Payload resource metering and throttling.
#[derive(Clone)]
pub struct ResourceMeteringConfig {
    /// Kill switch for the evaluator. Limits take effect from the schedule when
    /// this is set and the schedule is non-empty.
    pub enabled: bool,
    /// Startup schedule held by the builder.
    pub schedule: Arc<ResourceMeteringSchedule>,
    /// `meterBundle` results used to evaluate the schedule.
    pub provider: SharedMeteringProvider,
}

impl std::fmt::Debug for ResourceMeteringConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceMeteringConfig")
            .field("enabled", &self.enabled)
            .field("schedule_empty", &self.schedule.is_empty())
            .field("provider_enabled", &crate::MeteringProvider::is_enabled(self.provider.as_ref()))
            .finish()
    }
}

impl Default for ResourceMeteringConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: Arc::new(ResourceMeteringSchedule::default()),
            provider: Arc::new(NoopMeteringProvider),
        }
    }
}

impl ResourceMeteringConfig {
    /// Builds a shared config from startup flags.
    pub fn from_parts(
        enabled: bool,
        schedule_path: Option<&Path>,
        provider: SharedMeteringProvider,
    ) -> Result<Self, ResourceMeteringError> {
        let schedule = if enabled {
            match schedule_path {
                Some(path) => ResourceMeteringSchedule::from_file(path)?,
                None => ResourceMeteringSchedule::default(),
            }
        } else {
            if let Some(path) = schedule_path {
                warn!(
                    path = %path.display(),
                    "resource metering schedule is ignored because metering is disabled"
                );
            }
            ResourceMeteringSchedule::default()
        };
        Ok(Self { enabled, schedule: Arc::new(schedule), provider })
    }

    /// Returns whether metering is enabled with a non-empty schedule.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.schedule.is_empty()
    }

    /// Looks up the simulated sample for `tx_hash`, if the provider has a matching result.
    pub fn simulated_sample(&self, tx_hash: &TxHash) -> Option<ResourceSample> {
        if !self.is_active() {
            return None;
        }
        MeteringProvider::get(self.provider.as_ref(), tx_hash)
            .and_then(|meter| ResourceSample::from_meter(&meter, tx_hash))
    }

    /// Checks simulated `meterBundle` usage against the schedule.
    ///
    /// Payload builders call this before EVM execution and skip the transaction
    /// when [`ResourceThrottlingDecision::should_exclude`] is true. The returned
    /// sample is passed to [`Self::check_executed_usage`] after execution.
    /// Sequencer transactions use [`Self::unthrottled_usage`] /
    /// [`Self::account_unthrottled`] instead.
    ///
    /// [`ResourceThrottlingDecision::CalculationFailed`] is not recorded here:
    /// the transaction still executes, and [`Self::check_executed_usage`] records
    /// the final outcome, including a later calculation failure.
    /// This method does not add usage to the block totals.
    pub fn check_simulated_usage(
        &self,
        tx_hash: &TxHash,
        cumulative: &[u128],
    ) -> (Option<ResourceSample>, ResourceThrottlingDecision) {
        if !self.is_active() {
            return (None, ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(0)));
        }
        let simulated = self.simulated_sample(tx_hash);
        let decision = simulated.as_ref().map_or_else(
            || {
                ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(
                    self.schedule.dimensions.len(),
                ))
            },
            |sample| self.schedule.decide_sample(sample, cumulative),
        );
        if decision.should_exclude() {
            self.record_decision(tx_hash, &decision);
        }
        (simulated, decision)
    }

    /// Checks executed gas and post-state usage against the schedule.
    ///
    /// Overlays actual gas and post-state `STATE_*` counts on the simulated
    /// sample from [`Self::check_simulated_usage`], then runs the schedule.
    /// Records every outcome. Does not add usage to the block totals; callers
    /// that include the transaction pass [`ResourceThrottlingDecision::committed_usage`]
    /// to [`Self::apply_accounted_usage`].
    pub fn check_executed_usage(
        &self,
        tx_hash: &TxHash,
        gas_used: u64,
        state: &EvmState,
        simulated: Option<&ResourceSample>,
        cumulative: &[u128],
    ) -> ResourceThrottlingDecision {
        if !self.is_active() {
            return ResourceThrottlingDecision::Allow(ResourceMeteringUsage::zero(0));
        }
        let sample = ResourceSample::from_execution(gas_used, state, simulated);
        let decision = self.schedule.decide_sample(&sample, cumulative);
        self.record_decision(tx_hash, &decision);
        decision
    }

    /// Calculates usage for an unthrottled transaction without applying it.
    ///
    /// Evaluate failures fail open: the method records
    /// [`ResourceThrottlingDecision::CalculationFailed`] and returns `None`.
    pub fn unthrottled_usage(
        &self,
        tx_hash: &TxHash,
        gas_used: u64,
        state: &EvmState,
    ) -> Option<ResourceMeteringUsage> {
        if !self.is_active() {
            return None;
        }
        let simulated = self.simulated_sample(tx_hash);
        let sample = ResourceSample::from_execution(gas_used, state, simulated.as_ref());
        self.schedule.evaluate(sample.gas_used, &sample.operations).map_or_else(
            |_| {
                self.record_decision(tx_hash, &ResourceThrottlingDecision::CalculationFailed);
                None
            },
            Some,
        )
    }

    /// Accounts an unthrottled transaction into cumulative usage.
    ///
    /// Sequencer transactions always commit when execution succeeds.
    /// Calculation and apply failures fail open so a misconfigured schedule
    /// cannot halt payload construction.
    pub fn account_unthrottled(
        &self,
        tx_hash: &TxHash,
        gas_used: u64,
        state: &EvmState,
        cumulative: &mut Vec<u128>,
    ) {
        if let Some(usage) = self.unthrottled_usage(tx_hash, gas_used, state) {
            self.apply_accounted_usage(tx_hash, &usage, cumulative);
        }
    }

    /// Adds accounted usage to the block totals.
    ///
    /// Overflow fails open: the transaction stays included and this method
    /// records [`ResourceThrottlingDecision::CalculationFailed`] instead of
    /// aborting the payload.
    pub fn apply_accounted_usage(
        &self,
        tx_hash: &TxHash,
        usage: &ResourceMeteringUsage,
        cumulative: &mut Vec<u128>,
    ) {
        if usage.add_to(cumulative).is_err() {
            self.record_decision(tx_hash, &ResourceThrottlingDecision::CalculationFailed);
        }
    }

    fn record_decision(&self, tx_hash: &TxHash, decision: &ResourceThrottlingDecision) {
        match decision {
            ResourceThrottlingDecision::Allow(_) => {}
            ResourceThrottlingDecision::Throttle { error, .. } => {
                ResourceMeteringMetrics::record_limit(error, !error.dry_run);
                debug!(
                    target: "payload_builder",
                    tx_hash = %tx_hash,
                    dimension = %error.dimension,
                    scope = %error.scope,
                    used = error.used,
                    limit = error.limit,
                    "resource throttling budget exceeded"
                );
            }
            ResourceThrottlingDecision::CalculationFailed => {
                ResourceMeteringMetrics::calculation_failed().increment(1);
                warn!(
                    target: "payload_builder",
                    tx_hash = %tx_hash,
                    "resource metering usage calculation failed"
                );
            }
        }
    }
}

/// Contains the data availability configuration for the Base payload builder.
///
/// This type is shareable and can be used to update the DA configuration for the Base payload
/// builder.
#[derive(Debug, Clone, Default)]
pub struct BaseDAConfig {
    inner: Arc<BaseDAConfigInner>,
}

impl BaseDAConfig {
    /// Creates a new Data Availability configuration with the given maximum sizes.
    pub fn new(max_da_tx_size: u64, max_da_block_size: u64) -> Self {
        let this = Self::default();
        this.set_max_da_size(max_da_tx_size, max_da_block_size);
        this
    }

    /// Returns whether the configuration is empty.
    pub fn is_empty(&self) -> bool {
        self.max_da_tx_size().is_none() && self.max_da_block_size().is_none()
    }

    /// Returns the maximum allowed data availability size per transaction, if any.
    pub fn max_da_tx_size(&self) -> Option<u64> {
        let val = self.inner.max_da_tx_size.load(std::sync::atomic::Ordering::Relaxed);
        if val == 0 { None } else { Some(val) }
    }

    /// Returns the max allowed data availability size per block, if any.
    pub fn max_da_block_size(&self) -> Option<u64> {
        let val = self.inner.max_da_block_size.load(std::sync::atomic::Ordering::Relaxed);
        if val == 0 { None } else { Some(val) }
    }

    /// Sets the maximum data availability size currently allowed for inclusion. 0 means no maximum.
    pub fn set_max_da_size(&self, max_da_tx_size: u64, max_da_block_size: u64) {
        self.set_max_tx_size(max_da_tx_size);
        self.set_max_block_size(max_da_block_size);
    }

    /// Sets the maximum data availability size per transaction currently allowed for inclusion. 0
    /// means no maximum.
    pub fn set_max_tx_size(&self, max_da_tx_size: u64) {
        self.inner.max_da_tx_size.store(max_da_tx_size, std::sync::atomic::Ordering::Relaxed);
    }

    /// Sets the maximum data availability size per block currently allowed for inclusion. 0 means
    /// no maximum.
    pub fn set_max_block_size(&self, max_da_block_size: u64) {
        self.inner.max_da_block_size.store(max_da_block_size, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct BaseDAConfigInner {
    /// Don't include any transactions with data availability size larger than this in any built
    /// block
    ///
    /// 0 means no limit.
    max_da_tx_size: AtomicU64,
    /// Maximum total data availability size for a block
    ///
    /// 0 means no limit.
    max_da_block_size: AtomicU64,
}

/// Contains the gas-limit configuration for the Base payload builder.
///
/// This type is shareable and can be used to update the gas-limit configuration for the Base
/// payload
/// builder.
#[derive(Debug, Clone, Default)]
pub struct GasLimitConfig {
    /// Gas limit for a transaction
    ///
    /// 0 means use the default gas limit.
    gas_limit: Arc<AtomicU64>,
}

impl GasLimitConfig {
    /// Creates a new Gas Limit configuration with the given maximum gas limit.
    pub fn new(max_gas_limit: u64) -> Self {
        let this = Self::default();
        this.set_gas_limit(max_gas_limit);
        this
    }
    /// Returns the gas limit for a transaction, if any.
    pub fn gas_limit(&self) -> Option<u64> {
        let val = self.gas_limit.load(std::sync::atomic::Ordering::Relaxed);
        if val == 0 { None } else { Some(val) }
    }
    /// Sets the gas limit for a transaction. 0 means use the default gas limit.
    pub fn set_gas_limit(&self, gas_limit: u64) {
        self.gas_limit.store(gas_limit, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path};

    use alloy_primitives::TxHash;
    use base_bundles::{MeterBundleResponse, OpcodeGas, TransactionResult};
    use revm::state::EvmState;

    use super::*;
    use crate::{ResourceMeteringDimension, ResourceMeteringOperation};

    #[test]
    fn test_da() {
        let da = BaseDAConfig::default();
        assert_eq!(da.max_da_tx_size(), None);
        assert_eq!(da.max_da_block_size(), None);
        da.set_max_da_size(100, 200);
        assert_eq!(da.max_da_tx_size(), Some(100));
        assert_eq!(da.max_da_block_size(), Some(200));
        da.set_max_da_size(0, 0);
        assert_eq!(da.max_da_tx_size(), None);
        assert_eq!(da.max_da_block_size(), None);
    }

    #[test]
    fn test_da_constrained() {
        let config = BaseBuilderConfig::default();
        assert!(config.constrained_da_config().is_none());
    }

    #[test]
    fn new_preserves_manifest_precheck_setting() {
        let config =
            BaseBuilderConfig::new(BaseDAConfig::default(), GasLimitConfig::default(), false);
        assert!(!config.manifest_precheck_enabled);
    }

    #[test]
    fn test_gas_limit() {
        let gas_limit = GasLimitConfig::default();
        assert_eq!(gas_limit.gas_limit(), None);
        gas_limit.set_gas_limit(50000);
        assert_eq!(gas_limit.gas_limit(), Some(50000));
        gas_limit.set_gas_limit(0);
        assert_eq!(gas_limit.gas_limit(), None);
    }

    #[test]
    fn disabled_metering_ignores_schedule_path_without_reading_file() {
        let missing = Path::new("/this/path/does/not/exist/resource-metering.json");
        let config = ResourceMeteringConfig::from_parts(
            false,
            Some(missing),
            Arc::new(NoopMeteringProvider),
        )
        .expect("disabled metering must not read the schedule file");
        assert!(!config.enabled);
        assert!(config.schedule.is_empty());
        assert!(!config.is_active());
    }

    fn compiled_cpu_schedule() -> ResourceMeteringSchedule {
        ResourceMeteringSchedule::new(vec![ResourceMeteringDimension {
            name: "cpu".to_string(),
            block_limit: 1_000,
            transaction_limit: 1_000,
            base_gas_weight: 1,
            operations: vec![ResourceMeteringOperation {
                name: "SSTORE".to_string(),
                gas_used_weight: 0,
                count_cost: 10,
            }],
            dry_run: false,
        }])
        .compile()
        .unwrap()
    }

    #[test]
    fn check_simulated_usage_fails_open_without_meter_data() {
        let config = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(compiled_cpu_schedule()),
            provider: Arc::new(NoopMeteringProvider),
        };
        let (simulated, decision) = config.check_simulated_usage(&TxHash::ZERO, &[]);
        assert!(simulated.is_none());
        assert!(!decision.should_exclude());
    }

    #[derive(Debug)]
    struct MapProvider(std::sync::Mutex<std::collections::HashMap<TxHash, MeterBundleResponse>>);

    impl MeteringProvider for MapProvider {
        fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse> {
            self.0.lock().unwrap().get(tx_hash).cloned()
        }
    }

    #[test]
    fn unthrottled_usage_overlays_simulated_opcodes() {
        let tx_hash = TxHash::repeat_byte(0x42);
        let meter = MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used: 21_000,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: vec![OpcodeGas {
                    contract_address: Default::default(),
                    opcode: "SSTORE".to_string(),
                    count: 3,
                    gas_used: 0,
                }],
            }],
            ..Default::default()
        };
        let config = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(compiled_cpu_schedule()),
            provider: Arc::new(MapProvider(std::sync::Mutex::new(HashMap::from([(
                tx_hash, meter,
            )])))),
        };
        let usage = config
            .unthrottled_usage(&tx_hash, 21_000, &EvmState::default())
            .expect("active config should produce usage");
        assert_eq!(usage.values, vec![21_000 + 30]);
    }

    #[test]
    fn check_simulated_usage_excludes_in_enforce_but_not_dry_run() {
        let tx_hash = TxHash::repeat_byte(0x42);
        let meter = MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used: 21_000,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: Vec::new(),
            }],
            ..Default::default()
        };
        let provider: SharedMeteringProvider =
            Arc::new(MapProvider(std::sync::Mutex::new(HashMap::from([(tx_hash, meter)]))));
        let enforce = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(compiled_cpu_schedule()),
            provider: Arc::clone(&provider),
        };
        let (_, decision) = enforce.check_simulated_usage(&tx_hash, &[]);
        assert!(decision.should_exclude());

        let dry_run = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(
                ResourceMeteringSchedule::new(vec![ResourceMeteringDimension {
                    name: "cpu".to_string(),
                    block_limit: 1_000,
                    transaction_limit: 1_000,
                    base_gas_weight: 1,
                    operations: Vec::new(),
                    dry_run: true,
                }])
                .compile()
                .unwrap(),
            ),
            provider,
        };
        let (_, decision) = dry_run.check_simulated_usage(&tx_hash, &[]);
        assert!(!decision.should_exclude());
    }

    fn overflowing_schedule() -> ResourceMeteringSchedule {
        ResourceMeteringSchedule::new(vec![ResourceMeteringDimension {
            name: "cpu".to_string(),
            block_limit: 1,
            transaction_limit: 1,
            base_gas_weight: u64::MAX,
            operations: vec![ResourceMeteringOperation {
                name: "SSTORE".to_string(),
                gas_used_weight: u64::MAX,
                count_cost: 0,
            }],
            dry_run: false,
        }])
        .compile()
        .unwrap()
    }

    #[test]
    fn unthrottled_usage_fails_open_on_evaluate_overflow() {
        let tx_hash = TxHash::repeat_byte(0x42);
        let meter = MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used: u64::MAX,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: vec![OpcodeGas {
                    contract_address: Default::default(),
                    opcode: "SSTORE".to_string(),
                    count: 1,
                    gas_used: u64::MAX,
                }],
            }],
            ..Default::default()
        };
        let config = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(overflowing_schedule()),
            provider: Arc::new(MapProvider(std::sync::Mutex::new(HashMap::from([(
                tx_hash, meter,
            )])))),
        };
        assert!(config.unthrottled_usage(&tx_hash, u64::MAX, &EvmState::default()).is_none());
    }

    #[test]
    fn account_unthrottled_fails_open_on_add_overflow() {
        let config = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(compiled_cpu_schedule()),
            provider: Arc::new(NoopMeteringProvider),
        };
        let mut cumulative = vec![u128::MAX];
        config.account_unthrottled(&TxHash::ZERO, 21_000, &EvmState::default(), &mut cumulative);
        assert_eq!(cumulative, vec![u128::MAX]);
    }

    #[test]
    fn check_simulated_usage_fails_open_on_calculation_failure() {
        let tx_hash = TxHash::repeat_byte(0x42);
        let meter = MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: Default::default(),
                eth_sent_to_coinbase: Default::default(),
                from_address: Default::default(),
                gas_fees: Default::default(),
                gas_price: Default::default(),
                gas_used: u64::MAX,
                to_address: None,
                tx_hash,
                value: Default::default(),
                execution_time_us: 0,
                opcode_gas: vec![OpcodeGas {
                    contract_address: Default::default(),
                    opcode: "SSTORE".to_string(),
                    count: 1,
                    gas_used: u64::MAX,
                }],
            }],
            ..Default::default()
        };
        let config = ResourceMeteringConfig {
            enabled: true,
            schedule: Arc::new(overflowing_schedule()),
            provider: Arc::new(MapProvider(std::sync::Mutex::new(HashMap::from([(
                tx_hash, meter,
            )])))),
        };
        let (_, decision) = config.check_simulated_usage(&tx_hash, &[]);
        assert_eq!(decision, ResourceThrottlingDecision::CalculationFailed);
        assert!(!decision.should_exclude());
    }
}
