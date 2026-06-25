use alloy_provider::RootProvider;
use tracing::info;

use super::{
    UpgradeSignalApplySummary, UpgradeSignalRuntimeApplier, UpgradeSignalRuntimeValidation,
};
use crate::{
    AlloyUpgradeSignalReader, UpgradeSignalConfig, UpgradeSignalError, UpgradeSignalSchedule,
};

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
    /// Creates a runtime upgrade signal refresher.
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
