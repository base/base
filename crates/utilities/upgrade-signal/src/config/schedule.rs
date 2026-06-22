use alloy_primitives::{Address, U256};
use alloy_provider::RootProvider;
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_genesis::BaseUpgrade;
use tracing::info;

use super::{UpgradeSignalDefaults, UpgradeSignalMode};
use crate::{
    contract::AlloyUpgradeSignalReader,
    error::UpgradeSignalError,
    metrics::{UpgradeSignalMetricLayer, UpgradeSignalMetrics},
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Configuration for reading contract-backed upgrades from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Contract-backed upgrades to pass to the contract.
    pub upgrade_ids: Vec<BaseUpgrade>,
    /// Contract-backed upgrades allowed to mutate local upgrade schedules.
    pub apply_upgrade_ids: Vec<BaseUpgrade>,
    /// Local schedule mutation mode.
    pub mode: UpgradeSignalMode,
    /// L1 block tag used to read the contract.
    pub l1_block_tag: BlockNumberOrTag,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
    /// Whether execution-layer metric recording is enabled.
    pub el_metrics_enabled: bool,
    /// Whether consensus-layer metric recording is enabled.
    pub cl_metrics_enabled: bool,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for one contract-backed upgrade.
    pub fn new(contract_address: Address, upgrade_id: BaseUpgrade) -> Self {
        Self {
            contract_address,
            upgrade_ids: vec![upgrade_id],
            apply_upgrade_ids: vec![upgrade_id],
            mode: UpgradeSignalMode::MetricsOnly,
            l1_block_tag: BlockNumberOrTag::Finalized,
            node_protocol_version: U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION),
            el_metrics_enabled: false,
            cl_metrics_enabled: false,
        }
    }

    /// Creates a contract reader using this configuration's contract address and block tag.
    pub const fn reader(&self, l1_provider: RootProvider) -> AlloyUpgradeSignalReader {
        AlloyUpgradeSignalReader::new(l1_provider, self.contract_address)
            .with_block_tag(self.l1_block_tag)
    }

    /// Returns a copy of `schedule` filtered to the configured upgrades that may be applied
    /// locally.
    pub fn application_schedule(&self, schedule: &UpgradeSignalSchedule) -> UpgradeSignalSchedule {
        schedule.filtered_to_upgrade_ids(&self.apply_upgrade_ids)
    }

    /// Returns true if metric recording is enabled for `layer`.
    pub const fn metrics_enabled(&self, layer: UpgradeSignalMetricLayer) -> bool {
        match layer {
            UpgradeSignalMetricLayer::Execution => self.el_metrics_enabled,
            UpgradeSignalMetricLayer::Consensus => self.cl_metrics_enabled,
        }
    }

    /// Filters `layers` to the metric layers enabled in this config.
    pub fn enabled_metrics_layers(
        &self,
        layers: &[UpgradeSignalMetricLayer],
    ) -> Vec<UpgradeSignalMetricLayer> {
        layers.iter().copied().filter(|layer| self.metrics_enabled(*layer)).collect()
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
            return Err(UpgradeSignalError::missing_protocol_version(
                signal.upgrade_id.contract_id().to_string(),
            ));
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
            signal.upgrade_id.contract_id().to_string(),
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
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let enabled_metrics_layers = self.enabled_metrics_layers(metrics_layers);
        let schedule = reader
            .read_schedule_with_retries(
                &self.upgrade_ids,
                UpgradeSignalDefaults::READ_ATTEMPTS,
                UpgradeSignalDefaults::READ_BACKOFF,
                &enabled_metrics_layers,
            )
            .await?;

        UpgradeSignalMetrics::record_schedule_for_layers(&enabled_metrics_layers, &schedule);
        for signal in &schedule.signals {
            info!(
                target: "upgrade_signal",
                context = log_context,
                upgrade_id = %signal.upgrade_id.contract_id(),
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use alloy_rpc_types_eth::BlockNumberOrTag;
    use rstest::rstest;

    use super::*;
    use crate::state::{UpgradeSignal, UpgradeSignalSchedule};

    fn upgrade(upgrade_id: &str) -> BaseUpgrade {
        BaseUpgrade::from_contract_fork_name(upgrade_id).unwrap()
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn defaults_to_finalized_block_tag(#[case] upgrade_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(upgrade_id),
        );

        assert_eq!(config.l1_block_tag, BlockNumberOrTag::Finalized);
    }

    fn signal(protocol_version: U256) -> UpgradeSignal {
        UpgradeSignal {
            upgrade_id: BaseUpgrade::Azul,
            activation_timestamp: 42,
            protocol_version,
            l1_block_number: 1,
        }
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn accepts_signal_at_node_protocol_version(#[case] upgrade_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(upgrade_id),
        );

        assert!(
            config.validate_signal_protocol_version(&signal(config.node_protocol_version)).is_ok()
        );
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn rejects_signal_above_node_protocol_version(#[case] upgrade_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(upgrade_id),
        );
        let minimum_protocol_version = config.node_protocol_version + U256::from(1);

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(minimum_protocol_version)).unwrap_err(),
            crate::UpgradeSignalError::UnsupportedProtocolVersion { .. }
        ));
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn rejects_positive_signal_without_protocol_version(#[case] upgrade_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(upgrade_id),
        );

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(U256::ZERO)).unwrap_err(),
            crate::UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }

    fn malformed_read_only_schedule(config: &UpgradeSignalConfig) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(vec![
            signal(config.node_protocol_version),
            UpgradeSignal {
                upgrade_id: BaseUpgrade::Beryl,
                activation_timestamp: 5,
                protocol_version: U256::ZERO,
                l1_block_number: 1,
            },
        ])
    }

    #[test]
    fn read_validation_rejects_missing_protocol_version_on_read_only_upgrade() {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            BaseUpgrade::Azul,
        );
        let schedule = malformed_read_only_schedule(&config);

        assert!(matches!(
            config.validate_read_schedule_protocol_versions(&schedule).unwrap_err(),
            crate::UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }

    #[test]
    fn applied_validation_allows_unsupported_version_on_read_only_upgrade() {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            BaseUpgrade::Azul,
        );

        let schedule = UpgradeSignalSchedule::new(vec![
            UpgradeSignal {
                upgrade_id: BaseUpgrade::Azul,
                activation_timestamp: 42,
                protocol_version: config.node_protocol_version,
                l1_block_number: 1,
            },
            UpgradeSignal {
                upgrade_id: BaseUpgrade::Beryl,
                activation_timestamp: 42,
                protocol_version: config.node_protocol_version + U256::from(1),
                l1_block_number: 1,
            },
        ]);

        assert!(
            config
                .validate_applied_schedule_protocol_versions(
                    &config.application_schedule(&schedule)
                )
                .is_ok()
        );
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn applied_validation_allows_clear_with_unsupported_protocol_version(#[case] upgrade_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(upgrade_id),
        );
        let schedule = UpgradeSignalSchedule::new(vec![UpgradeSignal {
            upgrade_id: upgrade(upgrade_id),
            activation_timestamp: 0,
            protocol_version: config.node_protocol_version + U256::from(1),
            l1_block_number: 1,
        }]);

        assert!(config.validate_applied_schedule_protocol_versions(&schedule).is_ok());
    }
}
