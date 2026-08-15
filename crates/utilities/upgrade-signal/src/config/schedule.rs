use alloy_primitives::{Address, U256};
use alloy_provider::RootProvider;
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_genesis::{BaseUpgrade, UpgradeActivationSink};
use tracing::info;
use url::Url;

use super::{UpgradeSignalDefaults, UpgradeSignalMode};
use crate::{
    contract::AlloyUpgradeSignalReader,
    error::UpgradeSignalError,
    metrics::{UpgradeSignalMetricLayer, UpgradeSignalMetrics},
    runtime::{UpgradeSignalRuntimeApplier, UpgradeSignalRuntimeValidation},
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Configuration for reading contract-backed upgrades from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Contract-backed upgrades to pass to the contract.
    pub upgrade_ids: Vec<BaseUpgrade>,
    /// Local schedule mutation mode.
    pub mode: UpgradeSignalMode,
    /// L1 block tag used to read the contract.
    pub l1_block_tag: BlockNumberOrTag,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for one contract-backed upgrade.
    pub fn new(contract_address: Address, upgrade_id: BaseUpgrade) -> Self {
        Self {
            contract_address,
            upgrade_ids: vec![upgrade_id],
            mode: UpgradeSignalMode::MetricsOnly,
            l1_block_tag: BlockNumberOrTag::Finalized,
            node_protocol_version: U256::from(UpgradeSignalDefaults::NODE_PROTOCOL_VERSION),
        }
    }

    /// Creates a contract reader using this configuration's contract address and block tag.
    pub const fn reader(&self, l1_provider: RootProvider) -> AlloyUpgradeSignalReader {
        AlloyUpgradeSignalReader::new(l1_provider, self.contract_address)
            .with_block_tag(self.l1_block_tag)
    }

    /// Returns true if this node supports the minimum protocol version attached to `signal`.
    pub fn supports_signal_protocol_version(&self, signal: &UpgradeSignal) -> bool {
        signal.protocol_version <= self.node_protocol_version
    }

    /// Returns an error if a positive activation timestamp omits its minimum protocol version.
    ///
    /// This malformed-signal check applies to every signal read from L1.
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
    /// Signals that clear an upgrade (activation timestamp `0`) are always supported, so a node can
    /// process a clear for an upgrade it does not implement.
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

    /// Validates the minimum protocol version of every signal in the schedule (presence and
    /// support).
    pub fn validate_schedule_protocol_versions(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            self.validate_signal_protocol_version(signal)?;
        }

        Ok(())
    }

    /// Reads the L1 startup schedule, validates runtime invariants, and applies it to both sinks.
    ///
    /// Execution is applied before consensus so an execution-only validation failure leaves the
    /// rollup config unchanged.
    pub async fn apply_startup_to_sinks<EL, CL>(
        &self,
        l1_rpc: Url,
        log_context: &'static str,
        runtime_validation: UpgradeSignalRuntimeValidation,
        chain_id: u64,
        execution_sink: &mut EL,
        consensus_sink: &mut CL,
    ) -> eyre::Result<()>
    where
        EL: UpgradeActivationSink + Clone,
        EL::Error: std::error::Error + Send + Sync + 'static,
        CL: UpgradeActivationSink + Clone,
        CL::Error: std::error::Error + Send + Sync + 'static,
    {
        let reader = self.reader(RootProvider::new_http(l1_rpc));
        let schedule = self
            .read_validated_schedule(
                &reader,
                log_context,
                &[UpgradeSignalMetricLayer::Execution, UpgradeSignalMetricLayer::Consensus],
            )
            .await?;

        runtime_validation.validate_schedule(chain_id, &schedule)?;

        UpgradeSignalRuntimeApplier::apply_schedule_to_sink(chain_id, &schedule, execution_sink)
            .map_err(eyre::Report::new)?
            .log("execution chain spec");

        UpgradeSignalRuntimeApplier::apply_schedule_to_sink(chain_id, &schedule, consensus_sink)
            .map_err(eyre::Report::new)?
            .log("rollup config");

        Ok(())
    }

    /// Reads, records, logs, and validates the L1 schedule.
    pub async fn read_validated_schedule(
        &self,
        reader: &AlloyUpgradeSignalReader,
        log_context: &'static str,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let schedule = reader
            .read_schedule_with_retries(
                &self.upgrade_ids,
                UpgradeSignalDefaults::READ_ATTEMPTS,
                UpgradeSignalDefaults::READ_BACKOFF,
                metrics_layers,
            )
            .await?;

        UpgradeSignalMetrics::record_schedule_for_layers(metrics_layers, &schedule);
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

        self.validate_schedule_protocol_versions(&schedule)?;

        Ok(schedule)
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

    fn malformed_schedule(config: &UpgradeSignalConfig) -> UpgradeSignalSchedule {
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
    fn schedule_validation_rejects_missing_protocol_version() {
        let config = UpgradeSignalConfig::new(
            address!("0000000000000000000000000000000000000001"),
            BaseUpgrade::Azul,
        );
        let schedule = malformed_schedule(&config);

        assert!(matches!(
            config.validate_schedule_protocol_versions(&schedule).unwrap_err(),
            crate::UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }

    #[test]
    fn schedule_validation_rejects_unsupported_protocol_version() {
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

        assert!(matches!(
            config.validate_schedule_protocol_versions(&schedule).unwrap_err(),
            crate::UpgradeSignalError::UnsupportedProtocolVersion { .. }
        ));
    }

    #[rstest]
    #[case("azul")]
    #[case("beryl")]
    fn schedule_validation_allows_clear_with_unsupported_protocol_version(
        #[case] upgrade_id: &str,
    ) {
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

        assert!(config.validate_schedule_protocol_versions(&schedule).is_ok());
    }
}
