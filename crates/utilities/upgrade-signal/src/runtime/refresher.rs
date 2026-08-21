use super::{UpgradeSignalApplySummary, UpgradeSignalRuntimeApplier};
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
    /// Metric layer recorded by this refresher.
    pub metrics_layer: UpgradeSignalMetricLayer,
}

impl UpgradeSignalRefresher {
    /// Creates a runtime upgrade signal refresher.
    pub const fn new(
        config: UpgradeSignalConfig,
        reader: AlloyUpgradeSignalReader,
        chain_id: u64,
        metrics_layer: UpgradeSignalMetricLayer,
    ) -> Self {
        Self { config, reader, chain_id, metrics_layer }
    }

    /// Validates and applies an already-read schedule without touching L1.
    ///
    /// This is atomic: the whole schedule is validated before any registry mutation, so a
    /// validation failure leaves the runtime registry unchanged. The live poller
    /// ([`crate::UpgradeSignalMonitor::poll_and_apply`]) advances its applied baseline only when
    /// this call succeeds, so a failed apply leaves the schedule offered for retry on the next
    /// poll rather than being silently adopted as the baseline.
    pub fn apply(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<UpgradeSignalApplySummary, UpgradeSignalError> {
        self.config.validate_schedule_protocol_versions(schedule)?;
        let summary = UpgradeSignalRuntimeApplier::apply_schedule(self.chain_id, schedule);
        summary.log("runtime registry");

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

    fn refresher(chain_id: u64) -> UpgradeSignalRefresher {
        let config = UpgradeSignalConfig::new(Address::ZERO);
        let reader = config.reader("http://127.0.0.1:1".parse().unwrap()).unwrap();
        UpgradeSignalRefresher::new(config, reader, chain_id, UpgradeSignalMetricLayer::Consensus)
    }

    fn schedule(
        upgrade_id: BaseUpgrade,
        activation_timestamp: u64,
        protocol_version: U256,
    ) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            vec![UpgradeSignal { upgrade_id, activation_timestamp, protocol_version }],
        )
    }

    #[test]
    fn apply_applies_valid_schedule_to_registry() {
        let chain_id = 9_100_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let summary = refresher(chain_id)
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

        // Node supports 1.1.0; a 1.1.1 minimum is genuinely newer and must be rejected. Adding
        // `+ 1` to the dev-build sentinel no longer works: it decodes to a pre-release of the max
        // release, which now sorts below the release under the semver ordering.
        let mut config = UpgradeSignalConfig::new(Address::ZERO);
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let reader = config.reader("http://127.0.0.1:1".parse().unwrap()).unwrap();
        let refresher = UpgradeSignalRefresher::new(
            config,
            reader,
            chain_id,
            UpgradeSignalMetricLayer::Consensus,
        );

        let unsupported = UpgradeSignalDefaults::packed_protocol_version(1, 1, 1);
        refresher.apply(&schedule(BaseUpgrade::Azul, 42, unsupported)).unwrap_err();

        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);
    }
}
