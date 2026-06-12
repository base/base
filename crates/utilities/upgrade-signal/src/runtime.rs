//! Runtime upgrade signal application.

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use base_common_genesis::{
    HardForkActivation, HardForkActivationOverrides, HardForkActivationSink, HardForkConfig,
    RuntimeHardForkRegistry,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalError, UpgradeSignalSchedule,
};

/// Runtime action taken for one upgrade signal.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeSignalApplyAction {
    /// The hardfork timestamp was applied.
    Applied,
    /// The hardfork timestamp was cleared.
    Cleared,
    /// The hardfork ID is not supported by this node.
    Ignored,
}

/// Runtime application result for one upgrade signal.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpgradeSignalApplyChange {
    /// Hardfork ID read from the L1 contract.
    pub hardfork_id: String,
    /// Action taken for the hardfork ID.
    pub action: UpgradeSignalApplyAction,
    /// Activation timestamp read from the L1 contract.
    pub activation_timestamp: u64,
    /// Minimum node protocol version read from the L1 contract.
    pub minimum_protocol_version: String,
    /// L1 block number used for the contract read.
    pub l1_block_number: u64,
}

/// Runtime application summary for an upgrade signal schedule.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpgradeSignalApplySummary {
    /// L2 chain ID whose runtime fork view was updated.
    pub chain_id: u64,
    /// L1 block number used for the contract read.
    pub l1_block_number: Option<u64>,
    /// Number of configured hardfork signals read from L1.
    pub configured_hardforks: usize,
    /// Number of hardfork timestamps applied.
    pub applied_hardforks: usize,
    /// Number of hardfork timestamps cleared.
    pub cleared_hardforks: usize,
    /// Number of unsupported hardfork signals ignored.
    pub ignored_hardforks: usize,
    /// Per-hardfork application results.
    pub changes: Vec<UpgradeSignalApplyChange>,
}

impl UpgradeSignalApplySummary {
    /// Creates an empty runtime application summary.
    pub fn new(chain_id: u64, schedule: &UpgradeSignalSchedule) -> Self {
        Self {
            chain_id,
            l1_block_number: schedule.signals.iter().map(|signal| signal.l1_block_number).max(),
            configured_hardforks: schedule.signals.len(),
            applied_hardforks: 0,
            cleared_hardforks: 0,
            ignored_hardforks: 0,
            changes: Vec::new(),
        }
    }

    /// Logs each per-hardfork action and a summary line for an applied schedule.
    ///
    /// `target` names the destination the schedule was applied to (e.g. "rollup config").
    pub fn log(&self, target: &'static str) {
        for change in &self.changes {
            match change.action {
                UpgradeSignalApplyAction::Applied => info!(
                    target: "upgrade_signal",
                    destination = target,
                    hardfork_id = %change.hardfork_id,
                    activation_timestamp = change.activation_timestamp,
                    "applied upgrade signal"
                ),
                UpgradeSignalApplyAction::Cleared => info!(
                    target: "upgrade_signal",
                    destination = target,
                    hardfork_id = %change.hardfork_id,
                    "cleared upgrade signal"
                ),
                UpgradeSignalApplyAction::Ignored => debug!(
                    target: "upgrade_signal",
                    destination = target,
                    hardfork_id = %change.hardfork_id,
                    activation_timestamp = change.activation_timestamp,
                    "ignored unsupported upgrade signal"
                ),
            }
        }
        info!(
            target: "upgrade_signal",
            destination = target,
            chain_id = self.chain_id,
            l1_block_number = ?self.l1_block_number,
            applied_hardforks = self.applied_hardforks,
            cleared_hardforks = self.cleared_hardforks,
            ignored_hardforks = self.ignored_hardforks,
            configured_hardforks = self.configured_hardforks,
            "applied upgrade signal schedule"
        );
    }
}

/// Runtime schedule validation context shared by execution and consensus refresh paths.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct UpgradeSignalRuntimeValidation {
    /// Whether positive Beryl signals require an execution activation admin address.
    pub require_activation_admin_for_beryl: bool,
    /// Execution activation admin address for the L2 chain, when known.
    pub activation_admin_address: Option<Address>,
}

impl UpgradeSignalRuntimeValidation {
    /// Creates a validation context with execution-specific checks disabled.
    pub const fn disabled() -> Self {
        Self { require_activation_admin_for_beryl: false, activation_admin_address: None }
    }

    /// Creates a validation context that enforces execution activation admin invariants.
    pub const fn with_activation_admin_address(activation_admin_address: Option<Address>) -> Self {
        Self { require_activation_admin_for_beryl: true, activation_admin_address }
    }

    /// Creates the fail-closed validation context used when no activation admin source is known.
    ///
    /// This requires an activation admin address for positive Beryl signals but has none, so a
    /// positive Beryl signal is rejected rather than applied unguarded.
    pub const fn fail_closed() -> Self {
        Self::with_activation_admin_address(None)
    }

    /// Validates a schedule before it mutates the process-local runtime registry.
    pub fn validate_schedule(
        &self,
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        let positive_beryl_signal = schedule.signals.iter().any(|signal| {
            signal.positive_activation_timestamp().is_some()
                && HardForkConfig::canonical_hardfork_id(&signal.hardfork_id) == Some("beryl")
        });

        if self.require_activation_admin_for_beryl && positive_beryl_signal {
            match self.activation_admin_address {
                None => return Err(UpgradeSignalError::missing_activation_admin_address(chain_id)),
                Some(address) if address.is_zero() => {
                    return Err(UpgradeSignalError::zero_activation_admin_address(chain_id));
                }
                Some(_) => {}
            }
        }

        Ok(())
    }
}

impl Default for UpgradeSignalRuntimeValidation {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Hardfork activation sink backed by the process-local runtime registry for one chain.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistrySink {
    /// L2 chain ID whose runtime fork view is mutated.
    pub chain_id: u64,
    /// Buffered updates to apply to the runtime registry at finalize time.
    pub updates: HardForkActivationOverrides,
}

impl RuntimeRegistrySink {
    /// Creates a runtime registry sink for one chain.
    pub const fn new(chain_id: u64) -> Self {
        Self { chain_id, updates: HardForkActivationOverrides::new() }
    }
}

impl HardForkActivationSink for RuntimeRegistrySink {
    type Error = core::convert::Infallible;

    fn apply_activation(
        &mut self,
        hardfork_id: &str,
        activation: HardForkActivation,
    ) -> Result<bool, Self::Error> {
        Ok(self.updates.set_activation(hardfork_id, activation))
    }

    fn finalize(&mut self) -> Result<(), Self::Error> {
        let updates = core::mem::take(&mut self.updates);

        RuntimeHardForkRegistry::update_chain(self.chain_id, |overrides| {
            for (hardfork_id, activation) in updates.activations {
                overrides.set_activation(&hardfork_id, activation);
            }
        });

        Ok(())
    }
}

/// Applies upgrade signal schedules to any hardfork activation sink.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeSignalRuntimeApplier;

impl UpgradeSignalRuntimeApplier {
    /// Applies a schedule to any [`HardForkActivationSink`], returning an application summary.
    ///
    /// This stages the full batch on a cloned sink and only commits it back on success, so a
    /// failed later activation cannot leave earlier mutations partially applied.
    pub fn apply_schedule_to_sink<S: HardForkActivationSink + Clone>(
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
        sink: &mut S,
    ) -> Result<UpgradeSignalApplySummary, S::Error> {
        let mut summary = UpgradeSignalApplySummary::new(chain_id, schedule);
        let mut staged_sink = sink.clone();

        for signal in &schedule.signals {
            let activation =
                HardForkActivation::from_timestamp(signal.positive_activation_timestamp());
            let supported = staged_sink.apply_activation(&signal.hardfork_id, activation)?;

            let action = if !supported {
                UpgradeSignalApplyAction::Ignored
            } else if activation.timestamp().is_some() {
                UpgradeSignalApplyAction::Applied
            } else {
                UpgradeSignalApplyAction::Cleared
            };
            match action {
                UpgradeSignalApplyAction::Applied => summary.applied_hardforks += 1,
                UpgradeSignalApplyAction::Cleared => summary.cleared_hardforks += 1,
                UpgradeSignalApplyAction::Ignored => summary.ignored_hardforks += 1,
            }

            summary.changes.push(UpgradeSignalApplyChange {
                hardfork_id: signal.hardfork_id.clone(),
                action,
                activation_timestamp: signal.activation_timestamp,
                minimum_protocol_version: signal.protocol_version.to_string(),
                l1_block_number: signal.l1_block_number,
            });
        }

        staged_sink.finalize()?;
        *sink = staged_sink;

        Ok(summary)
    }

    /// Applies a schedule to the runtime hardfork registry for one chain.
    pub fn apply_schedule(
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
    ) -> UpgradeSignalApplySummary {
        let mut sink = RuntimeRegistrySink::new(chain_id);
        Self::apply_schedule_to_sink(chain_id, schedule, &mut sink)
            .unwrap_or_else(|never| match never {})
    }
}

/// Reads and applies upgrade signal schedules while the node is running.
#[derive(Debug, Clone)]
pub struct UpgradeSignalRefresher {
    /// Shared upgrade signal schedule read configuration.
    pub config: UpgradeSignalConfig,
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// L2 chain ID whose runtime fork view is updated.
    pub chain_id: u64,
    /// Runtime schedule validation context.
    pub runtime_validation: UpgradeSignalRuntimeValidation,
}

impl UpgradeSignalRefresher {
    /// Creates a runtime upgrade signal refresher with an explicit validation context.
    ///
    /// The validation context is required, not defaulted, so every caller consciously chooses
    /// whether (and how) runtime schedules are validated before they mutate the registry.
    pub const fn new(
        config: UpgradeSignalConfig,
        l1_provider: RootProvider,
        chain_id: u64,
        runtime_validation: UpgradeSignalRuntimeValidation,
    ) -> Self {
        let reader = AlloyUpgradeSignalReader::new(l1_provider, config.contract_address)
            .with_block_tag(config.l1_block_tag);
        Self { config, reader, chain_id, runtime_validation }
    }

    /// Reads, metrics-records, logs, and validates the current L1 schedule.
    pub async fn read_validated_schedule(
        &self,
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let application_schedule = self
            .config
            .read_validated_application_schedule(&self.reader, "runtime refresh")
            .await?;
        self.runtime_validation.validate_schedule(self.chain_id, &application_schedule)?;

        Ok(application_schedule)
    }

    /// Reads, validates, metrics-records, logs, and applies the current L1 schedule.
    pub async fn refresh(&self) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        let schedule = self.read_validated_schedule().await?;
        let summary = UpgradeSignalRuntimeApplier::apply_schedule(self.chain_id, &schedule);
        info!(
            target: "upgrade_signal",
            chain_id = summary.chain_id,
            l1_block_number = ?summary.l1_block_number,
            applied_hardforks = summary.applied_hardforks,
            cleared_hardforks = summary.cleared_hardforks,
            ignored_hardforks = summary.ignored_hardforks,
            configured_hardforks = summary.configured_hardforks,
            "applied runtime upgrade signal schedule"
        );

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_genesis::{HardForkActivationSink, RuntimeHardForkRegistry};

    use super::*;
    use crate::UpgradeSignal;

    fn schedule(signals: &[(&str, u64)]) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            signals
                .iter()
                .map(|(hardfork_id, activation_timestamp)| UpgradeSignal {
                    hardfork_id: hardfork_id.to_string(),
                    activation_timestamp: *activation_timestamp,
                    protocol_version: U256::from(7),
                    l1_block_number: 11,
                })
                .collect(),
        )
    }

    #[test]
    fn applies_runtime_schedule() {
        let chain_id = 9_000_001;
        RuntimeHardForkRegistry::clear_chain(chain_id);

        let summary = UpgradeSignalRuntimeApplier::apply_schedule(
            chain_id,
            &schedule(&[("azul", 42), ("beryl", 0), ("unknown", 10)]),
        );

        assert_eq!(summary.applied_hardforks, 1);
        assert_eq!(summary.cleared_hardforks, 1);
        assert_eq!(summary.ignored_hardforks, 1);
        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "azul"),
            Some(HardForkActivation::Timestamp(42))
        );
        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "beryl"),
            Some(HardForkActivation::Never)
        );
        assert_eq!(RuntimeHardForkRegistry::activation(chain_id, "unknown"), None);

        RuntimeHardForkRegistry::clear_chain(chain_id);
    }

    #[test]
    fn default_hardfork_ids_match_canonical_contract_ids() {
        assert_eq!(
            crate::DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS,
            HardForkConfig::CONTRACT_HARDFORK_IDS
        );
    }

    #[test]
    fn validation_rejects_positive_beryl_without_activation_admin() {
        let validation = UpgradeSignalRuntimeValidation::with_activation_admin_address(None);

        let error =
            validation.validate_schedule(9_000_002, &schedule(&[("beryl", 42)])).unwrap_err();

        assert_eq!(
            error.to_string(),
            "missing activation admin address for Beryl-enabled chain ID: 9000002"
        );
    }

    #[test]
    fn validation_allows_cleared_beryl_without_activation_admin() {
        let validation = UpgradeSignalRuntimeValidation::with_activation_admin_address(None);

        validation.validate_schedule(9_000_003, &schedule(&[("beryl", 0)])).unwrap();
    }

    #[test]
    fn validation_rejects_positive_beryl_with_zero_activation_admin() {
        let validation =
            UpgradeSignalRuntimeValidation::with_activation_admin_address(Some(Address::ZERO));

        let error =
            validation.validate_schedule(9_000_003, &schedule(&[("beryl", 42)])).unwrap_err();

        assert_eq!(
            error.to_string(),
            "activation admin address must not be zero for Beryl-enabled chain ID: 9000003"
        );
    }

    #[test]
    fn disabled_validation_allows_positive_beryl_without_activation_admin() {
        UpgradeSignalRuntimeValidation::disabled()
            .validate_schedule(9_000_004, &schedule(&[("beryl", 42)]))
            .unwrap();
    }

    #[test]
    fn fail_closed_validation_rejects_positive_beryl() {
        assert!(
            UpgradeSignalRuntimeValidation::fail_closed()
                .validate_schedule(9_000_005, &schedule(&[("beryl", 42)]))
                .is_err()
        );
    }

    #[test]
    fn fail_closed_validation_allows_non_beryl_and_cleared_beryl() {
        UpgradeSignalRuntimeValidation::fail_closed()
            .validate_schedule(9_000_006, &schedule(&[("azul", 42), ("beryl", 0)]))
            .unwrap();
    }

    #[derive(Debug, Clone, Default, Eq, PartialEq)]
    struct RecordingSink {
        applied: Vec<(String, HardForkActivation)>,
        fail_on_hardfork_id: Option<String>,
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    struct RecordingSinkError;

    impl HardForkActivationSink for RecordingSink {
        type Error = RecordingSinkError;

        fn apply_activation(
            &mut self,
            hardfork_id: &str,
            activation: HardForkActivation,
        ) -> Result<bool, Self::Error> {
            if self.fail_on_hardfork_id.as_deref() == Some(hardfork_id) {
                return Err(RecordingSinkError);
            }

            self.applied.push((hardfork_id.to_string(), activation));
            Ok(true)
        }
    }

    #[test]
    fn apply_schedule_to_sink_is_transactional() {
        let mut sink = RecordingSink {
            applied: vec![("existing".to_string(), HardForkActivation::Timestamp(1))],
            fail_on_hardfork_id: Some("beryl".to_string()),
        };

        let error = UpgradeSignalRuntimeApplier::apply_schedule_to_sink(
            9_000_007,
            &schedule(&[("azul", 42), ("beryl", 84)]),
            &mut sink,
        )
        .unwrap_err();

        assert_eq!(error, RecordingSinkError);
        assert_eq!(sink.applied, vec![("existing".to_string(), HardForkActivation::Timestamp(1))]);
    }

    #[test]
    fn runtime_registry_sink_only_flushes_in_finalize() {
        let chain_id = 9_000_008;
        RuntimeHardForkRegistry::clear_chain(chain_id);
        let mut sink = RuntimeRegistrySink::new(chain_id);

        sink.apply_activation("azul", HardForkActivation::Timestamp(42)).unwrap();

        assert_eq!(RuntimeHardForkRegistry::activation(chain_id, "azul"), None);

        sink.finalize().unwrap();

        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "azul"),
            Some(HardForkActivation::Timestamp(42))
        );

        RuntimeHardForkRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_sink_merges_with_existing_overrides() {
        let chain_id = 9_000_009;
        RuntimeHardForkRegistry::clear_chain(chain_id);
        RuntimeHardForkRegistry::set_activation_timestamp(chain_id, "cobalt", 84);

        let mut sink = RuntimeRegistrySink::new(chain_id);
        sink.apply_activation("azul", HardForkActivation::Timestamp(42)).unwrap();
        sink.finalize().unwrap();

        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "azul"),
            Some(HardForkActivation::Timestamp(42))
        );
        assert_eq!(
            RuntimeHardForkRegistry::activation(chain_id, "cobalt"),
            Some(HardForkActivation::Timestamp(84))
        );

        RuntimeHardForkRegistry::clear_chain(chain_id);
    }
}
