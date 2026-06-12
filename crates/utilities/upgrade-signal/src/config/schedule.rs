use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::BlockNumberOrTag;
use tracing::info;

use super::{UpgradeSignalDefaults, UpgradeSignalMode};
use crate::{
    contract::AlloyUpgradeSignalReader,
    error::UpgradeSignalError,
    metrics::UpgradeSignalMetrics,
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Configuration for reading hardfork IDs from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Hardfork IDs to pass to the contract.
    pub hardfork_ids: Vec<String>,
    /// Hardfork IDs allowed to mutate local fork schedules.
    pub apply_hardfork_ids: Vec<String>,
    /// Local schedule mutation mode.
    pub mode: UpgradeSignalMode,
    /// L1 block tag used to read the contract.
    pub l1_block_tag: BlockNumberOrTag,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for one hardfork ID.
    pub fn new(contract_address: Address, hardfork_id: impl Into<String>) -> Self {
        let hardfork_id = hardfork_id.into();
        Self {
            contract_address,
            hardfork_ids: vec![hardfork_id.clone()],
            apply_hardfork_ids: vec![hardfork_id],
            mode: UpgradeSignalMode::MetricsOnly,
            l1_block_tag: BlockNumberOrTag::Finalized,
            node_protocol_version: U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION),
        }
    }

    /// Returns a copy of `schedule` filtered to the hardfork IDs that may be applied locally.
    pub fn application_schedule(&self, schedule: &UpgradeSignalSchedule) -> UpgradeSignalSchedule {
        schedule.filtered_to_hardfork_ids(&self.apply_hardfork_ids)
    }

    /// Returns true if this node supports the minimum protocol version attached to `signal`.
    pub fn supports_signal_protocol_version(&self, signal: &UpgradeSignal) -> bool {
        signal.protocol_version <= self.node_protocol_version
    }

    /// Returns an error if a positive activation timestamp omits its minimum protocol version.
    ///
    /// This malformed-signal check applies to every signal read from L1, including signals that
    /// this node only observes (reads) but does not apply.
    pub fn validate_signal_has_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        if signal.activation_timestamp > 0 && signal.protocol_version == U256::ZERO {
            return Err(UpgradeSignalError::missing_protocol_version(signal.hardfork_id.clone()));
        }

        Ok(())
    }

    /// Returns an error if this binary cannot support the signal's minimum protocol version.
    ///
    /// This capability check applies only to signals this node will apply, so a node can observe
    /// a future upgrade that requires newer software without aborting.
    pub fn validate_signal_supported_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        if signal.activation_timestamp == 0 {
            return Ok(());
        }

        if self.supports_signal_protocol_version(signal) {
            return Ok(());
        }

        Err(UpgradeSignalError::unsupported_protocol_version(
            signal.hardfork_id.clone(),
            signal.protocol_version,
            self.node_protocol_version,
        ))
    }

    /// Validates the minimum protocol version attached to one signal (presence and support).
    pub fn validate_signal_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        self.validate_signal_has_protocol_version(signal)?;
        self.validate_signal_supported_protocol_version(signal)
    }

    /// Validates that every positive signal in the full read schedule carries a protocol version.
    pub fn validate_read_schedule_protocol_versions(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            self.validate_signal_has_protocol_version(signal)?;
        }

        Ok(())
    }

    /// Validates that this binary supports every applied signal's minimum protocol version.
    pub fn validate_applied_schedule_protocol_versions(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            self.validate_signal_supported_protocol_version(signal)?;
        }

        Ok(())
    }

    /// Reads the L1 schedule, records metrics, logs each signal, validates it, and returns the
    /// application-filtered schedule ready to apply.
    ///
    /// This is the single read pipeline shared by startup application and runtime refresh. The
    /// malformed-signal check runs over the full read schedule; the protocol-version support check
    /// runs only over the schedule this node will apply.
    pub async fn read_validated_application_schedule(
        &self,
        reader: &AlloyUpgradeSignalReader,
        log_context: &'static str,
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let schedule = reader
            .read_schedule_with_retries(
                &self.hardfork_ids,
                UpgradeSignalDefaults::READ_ATTEMPTS,
                UpgradeSignalDefaults::READ_BACKOFF,
            )
            .await?;

        UpgradeSignalMetrics::record_schedule(&schedule);
        for signal in &schedule.signals {
            info!(
                target: "upgrade_signal",
                context = log_context,
                hardfork_id = %signal.hardfork_id,
                activation_timestamp = signal.activation_timestamp,
                minimum_protocol_version = %signal.protocol_version,
                node_protocol_version = %self.node_protocol_version,
                l1_block_number = signal.l1_block_number,
                "read dynamic upgrade signal"
            );
        }

        self.validate_read_schedule_protocol_versions(&schedule)?;
        let application_schedule = self.application_schedule(&schedule);
        self.validate_applied_schedule_protocol_versions(&application_schedule)?;

        Ok(application_schedule)
    }
}
