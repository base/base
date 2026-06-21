use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_genesis::BaseUpgrade;
use tracing::info;

use super::{UpgradeSignalDefaults, UpgradeSignalMode};
use crate::{
    contract::AlloyUpgradeSignalReader,
    error::UpgradeSignalError,
    metrics::UpgradeSignalMetrics,
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Configuration for reading contract-backed upgrades from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Contract-backed upgrades to pass to the contract.
    pub hardfork_ids: Vec<BaseUpgrade>,
    /// Contract-backed upgrades allowed to mutate local fork schedules.
    pub apply_hardfork_ids: Vec<BaseUpgrade>,
    /// Local schedule mutation mode.
    pub mode: UpgradeSignalMode,
    /// L1 block tag used to read the contract.
    pub l1_block_tag: BlockNumberOrTag,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for one contract-backed upgrade.
    pub fn new(contract_address: Address, hardfork_id: BaseUpgrade) -> Self {
        Self {
            contract_address,
            hardfork_ids: vec![hardfork_id],
            apply_hardfork_ids: vec![hardfork_id],
            mode: UpgradeSignalMode::MetricsOnly,
            l1_block_tag: BlockNumberOrTag::Finalized,
            node_protocol_version: U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION),
        }
    }

    /// Returns a copy of `schedule` filtered to the configured upgrades that may be applied
    /// locally.
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
            return Err(UpgradeSignalError::missing_protocol_version(
                signal.hardfork_id.contract_id().to_string(),
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
            signal.hardfork_id.contract_id().to_string(),
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
                hardfork_id = %signal.hardfork_id.contract_id(),
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

    fn upgrade(hardfork_id: &str) -> BaseUpgrade {
        BaseUpgrade::from_contract_fork_name(hardfork_id).unwrap()
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn defaults_to_finalized_block_tag(#[case] hardfork_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(hardfork_id),
        );

        assert_eq!(config.l1_block_tag, BlockNumberOrTag::Finalized);
    }

    fn signal(protocol_version: U256) -> UpgradeSignal {
        UpgradeSignal {
            hardfork_id: BaseUpgrade::Azul,
            activation_timestamp: 42,
            protocol_version,
            l1_block_number: 1,
        }
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn accepts_signal_at_node_protocol_version(#[case] hardfork_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(hardfork_id),
        );

        assert!(
            config.validate_signal_protocol_version(&signal(config.node_protocol_version)).is_ok()
        );
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn rejects_signal_above_node_protocol_version(#[case] hardfork_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(hardfork_id),
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
    fn rejects_positive_signal_without_protocol_version(#[case] hardfork_id: &str) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(hardfork_id),
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
                hardfork_id: BaseUpgrade::Beryl,
                activation_timestamp: 5,
                protocol_version: U256::ZERO,
                l1_block_number: 1,
            },
        ])
    }

    #[test]
    fn read_validation_rejects_missing_protocol_version_on_read_only_fork() {
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
    fn applied_validation_allows_unsupported_version_on_read_only_fork() {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            BaseUpgrade::Azul,
        );

        let schedule = UpgradeSignalSchedule::new(vec![
            UpgradeSignal {
                hardfork_id: BaseUpgrade::Azul,
                activation_timestamp: 42,
                protocol_version: config.node_protocol_version,
                l1_block_number: 1,
            },
            UpgradeSignal {
                hardfork_id: BaseUpgrade::Beryl,
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
    fn applied_validation_allows_clear_with_unsupported_protocol_version(
        #[case] hardfork_id: &str,
    ) {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            upgrade(hardfork_id),
        );
        let schedule = UpgradeSignalSchedule::new(vec![UpgradeSignal {
            hardfork_id: upgrade(hardfork_id),
            activation_timestamp: 0,
            protocol_version: config.node_protocol_version + U256::from(1),
            l1_block_number: 1,
        }]);

        assert!(config.validate_applied_schedule_protocol_versions(&schedule).is_ok());
    }
}
