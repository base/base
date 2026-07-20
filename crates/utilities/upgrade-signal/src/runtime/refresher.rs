use alloy_provider::RootProvider;
use tracing::info;

use super::{
    UpgradeSignalApplySummary, UpgradeSignalRuntimeApplier, UpgradeSignalRuntimeValidation,
};
use crate::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalError, UpgradeSignalMetricLayer,
    UpgradeSignalSchedule,
};

/// Reads and applies upgrade signal schedules while the node is running.
#[derive(Debug, Clone)]
pub struct UpgradeSignalRefresher {
    /// Shared upgrade signal schedule read configuration.
    pub config: UpgradeSignalConfig,
    /// L1 upgrade signal reader.
    pub reader: AlloyUpgradeSignalReader,
    /// L2 chain ID whose runtime upgrade view is updated.
    pub chain_id: u64,
    /// Runtime schedule validation context.
    pub runtime_validation: UpgradeSignalRuntimeValidation,
    /// Metric layer recorded by this refresher.
    pub metrics_layer: UpgradeSignalMetricLayer,
}

impl UpgradeSignalRefresher {
    /// Creates a runtime upgrade signal refresher.
    pub const fn new(
        config: UpgradeSignalConfig,
        l1_provider: RootProvider,
        chain_id: u64,
        runtime_validation: UpgradeSignalRuntimeValidation,
        metrics_layer: UpgradeSignalMetricLayer,
    ) -> Self {
        let reader = config.reader(l1_provider);
        Self { config, reader, chain_id, runtime_validation, metrics_layer }
    }

    /// Validates and applies an already-read schedule without touching L1.
    ///
    /// Callers do not retry failures: validation failures are deterministic, and failed reads
    /// never advance the monitor baseline, so changed signals are re-detected next poll.
    pub fn apply(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        self.config.validate_schedule_protocol_versions(schedule)?;
        self.runtime_validation.validate_schedule(self.chain_id, schedule)?;
        let summary = UpgradeSignalRuntimeApplier::apply_schedule(self.chain_id, schedule);
        info!(
            target: "upgrade_signal",
            chain_id = summary.chain_id,
            l1_block_number = ?summary.l1_block_number,
            applied_upgrades = summary.applied_upgrades,
            cleared_upgrades = summary.cleared_upgrades,
            ignored_upgrades = summary.ignored_upgrades,
            configured_upgrades = summary.configured_upgrades,
            "applied runtime upgrade signal schedule"
        );

        Ok(summary)
    }

    /// Reads the current L1 schedule with retries, recording this refresher's metric layer.
    pub async fn read_schedule(&self) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        self.config.read_schedule(&self.reader, "runtime refresh", &[self.metrics_layer]).await
    }

    /// Reads, metrics-records, logs, and applies the current L1 schedule.
    ///
    /// Validation happens once in [`Self::apply`].
    pub async fn refresh(&self) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        let schedule = self.read_schedule().await?;
        self.apply(&schedule)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation};

    use super::*;
    use crate::{UpgradeSignal, UpgradeSignalDefaults};

    fn refresher(
        chain_id: u64,
        runtime_validation: UpgradeSignalRuntimeValidation,
    ) -> UpgradeSignalRefresher {
        UpgradeSignalRefresher::new(
            UpgradeSignalConfig::new(Address::ZERO, BaseUpgrade::Azul),
            RootProvider::new_http("http://127.0.0.1:1".parse().unwrap()),
            chain_id,
            runtime_validation,
            UpgradeSignalMetricLayer::Consensus,
        )
    }

    fn schedule(
        upgrade_id: BaseUpgrade,
        activation_timestamp: u64,
        protocol_version: U256,
    ) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(vec![UpgradeSignal {
            upgrade_id,
            activation_timestamp,
            protocol_version,
            l1_block_number: 1,
        }])
    }

    #[test]
    fn apply_applies_valid_schedule_to_registry() {
        let chain_id = 9_100_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let summary = refresher(chain_id, UpgradeSignalRuntimeValidation::disabled())
            .apply(&schedule(BaseUpgrade::Azul, 42, UpgradeSignalDefaults::node_protocol_version()))
            .unwrap();

        assert_eq!(summary.applied_upgrades, 1);
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn apply_rejects_unsupported_protocol_version_without_mutating_registry() {
        let chain_id = 9_100_002;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let unsupported = UpgradeSignalDefaults::node_protocol_version() + U256::from(1);
        refresher(chain_id, UpgradeSignalRuntimeValidation::disabled())
            .apply(&schedule(BaseUpgrade::Azul, 42, unsupported))
            .unwrap_err();

        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);
    }

    #[test]
    fn apply_rejects_positive_beryl_schedule_when_fail_closed_without_mutating_registry() {
        let chain_id = 9_100_003;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        refresher(chain_id, UpgradeSignalRuntimeValidation::fail_closed())
            .apply(&schedule(
                BaseUpgrade::Beryl,
                42,
                UpgradeSignalDefaults::node_protocol_version(),
            ))
            .unwrap_err();

        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl), None);
    }
}
