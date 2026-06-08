//! Runtime upgrade signal application.

use alloy_provider::RootProvider;
use base_common_genesis::{HardForkActivation, RuntimeHardForkRegistry};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalError, UpgradeSignalMetrics,
    UpgradeSignalSchedule,
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
}

/// Applies upgrade signal schedules to the process-local runtime hardfork registry.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeSignalRuntimeApplier;

impl UpgradeSignalRuntimeApplier {
    /// Applies a schedule to the runtime hardfork registry for one chain.
    pub fn apply_schedule(
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
    ) -> UpgradeSignalApplySummary {
        let mut summary = UpgradeSignalApplySummary::new(chain_id, schedule);

        for signal in &schedule.signals {
            let (action, supported) =
                if let Some(timestamp) = signal.positive_activation_timestamp() {
                    (
                        UpgradeSignalApplyAction::Applied,
                        RuntimeHardForkRegistry::set_activation_timestamp(
                            chain_id,
                            &signal.hardfork_id,
                            timestamp,
                        ),
                    )
                } else {
                    (
                        UpgradeSignalApplyAction::Cleared,
                        RuntimeHardForkRegistry::set_activation(
                            chain_id,
                            &signal.hardfork_id,
                            HardForkActivation::Never,
                        ),
                    )
                };

            let action = if supported { action } else { UpgradeSignalApplyAction::Ignored };
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

        summary
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
}

impl UpgradeSignalRefresher {
    /// Creates a runtime upgrade signal refresher.
    pub const fn new(
        config: UpgradeSignalConfig,
        l1_provider: RootProvider,
        chain_id: u64,
    ) -> Self {
        let reader = AlloyUpgradeSignalReader::new(l1_provider, config.contract_address);
        Self { config, reader, chain_id }
    }

    /// Reads, metrics-records, logs, and validates the current L1 schedule.
    pub async fn read_validated_schedule(
        &self,
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let schedule = match self.reader.read_schedule(&self.config.hardfork_ids).await {
            Ok(schedule) => schedule,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors(&self.config.hardfork_ids);
                return Err(error);
            }
        };

        UpgradeSignalMetrics::record_schedule(&schedule);
        for signal in &schedule.signals {
            info!(
                target: "upgrade_signal",
                chain_id = self.chain_id,
                hardfork_id = %signal.hardfork_id,
                activation_timestamp = signal.activation_timestamp,
                minimum_protocol_version = %signal.protocol_version,
                node_protocol_version = %self.config.node_protocol_version,
                l1_block_number = signal.l1_block_number,
                "read dynamic upgrade signal for runtime refresh"
            );
        }
        let application_schedule = self.config.application_schedule(&schedule);
        self.config.validate_schedule_protocol_versions(&application_schedule)?;

        Ok(application_schedule)
    }

    /// Reads, validates, metrics-records, logs, and applies the current L1 schedule.
    pub async fn refresh(&self) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        let schedule = self.read_validated_schedule().await?;
        let summary =
            UpgradeSignalRuntimeApplier::apply_schedule(self.chain_id, &schedule);
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
    use alloy_primitives::U256;
    use base_common_genesis::{HardForkConfig, RuntimeHardForkRegistry};

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
}
