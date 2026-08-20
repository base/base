use core::time::Duration;

use alloy_primitives::{Address, U256};
use base_common_genesis::UpgradeActivationSink;
use tracing::info;
use url::Url;

use super::{UpgradeSignalBlockTag, UpgradeSignalDefaults, UpgradeSignalMode};
use crate::{
    PackedProtocolVersion,
    contract::AlloyUpgradeSignalReader,
    error::UpgradeSignalError,
    metrics::{UpgradeSignalMetricLayer, UpgradeSignalMetrics},
    runtime::UpgradeSignalRuntimeApplier,
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Configuration for reading contract-backed upgrades from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Local schedule mutation mode.
    pub mode: UpgradeSignalMode,
    /// L1 block tag used to read the contract. Also selects the live read poll interval.
    pub l1_block_tag: UpgradeSignalBlockTag,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
    /// Total deadline applied to every L1 schedule request.
    pub request_timeout: Duration,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for the full contract-backed upgrade set.
    pub fn new(contract_address: Address) -> Self {
        Self {
            contract_address,
            mode: UpgradeSignalMode::MetricsOnly,
            l1_block_tag: UpgradeSignalBlockTag::Finalized,
            node_protocol_version: UpgradeSignalDefaults::node_protocol_version(),
            request_timeout: UpgradeSignalDefaults::REQUEST_TIMEOUT,
        }
    }

    /// Creates a hardened contract reader using this configuration's contract address and block
    /// tag.
    pub fn reader(&self, l1_rpc: Url) -> Result<AlloyUpgradeSignalReader, UpgradeSignalError> {
        Ok(AlloyUpgradeSignalReader::new(l1_rpc, self.contract_address, self.request_timeout)?
            .with_block_tag(self.l1_block_tag.block_number_or_tag()))
    }

    /// Returns true if this node supports the minimum protocol version attached to `signal`.
    ///
    /// Compatibility compares the packed versions by their semver ordering (see
    /// [`PackedProtocolVersion`]), not as raw integers: an unrecognized version-type ranks above
    /// everything (fail-closed), then `major.minor.patch`, with a pre-release sorting below its
    /// matching release and `build`/reserved bits ignored.
    pub fn supports_signal_protocol_version(&self, signal: &UpgradeSignal) -> bool {
        PackedProtocolVersion::new(signal.protocol_version)
            <= PackedProtocolVersion::new(self.node_protocol_version)
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

    /// Reads the L1 startup schedule and applies it to both sinks.
    ///
    /// Execution is applied before consensus so an execution-only validation failure leaves the
    /// rollup config unchanged.
    pub async fn apply_startup_to_sinks<EL, CL>(
        &self,
        l1_rpc: Url,
        log_context: &'static str,
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
        let reader = self.reader(l1_rpc)?;
        let schedule = self
            .read_validated_schedule(
                &reader,
                log_context,
                &[UpgradeSignalMetricLayer::Execution, UpgradeSignalMetricLayer::Consensus],
            )
            .await?;

        UpgradeSignalRuntimeApplier::apply_schedule_to_sink(chain_id, &schedule, execution_sink)
            .map_err(eyre::Report::new)?
            .log("execution chain spec");

        UpgradeSignalRuntimeApplier::apply_schedule_to_sink(chain_id, &schedule, consensus_sink)
            .map_err(eyre::Report::new)?
            .log("rollup config");

        Ok(())
    }

    /// Reads the L1 schedule with retries, recording metrics and logging each signal.
    pub async fn read_schedule(
        &self,
        reader: &AlloyUpgradeSignalReader,
        log_context: &'static str,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let schedule = reader
            .read_schedule_with_retries(
                UpgradeSignalDefaults::READ_ATTEMPTS,
                UpgradeSignalDefaults::READ_BACKOFF,
                UpgradeSignalDefaults::READ_MAX_BACKOFF,
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
                l1_block_number = schedule.l1_block_number,
                "read dynamic upgrade signal"
            );
        }

        Ok(schedule)
    }

    /// Reads the L1 schedule via [`Self::read_schedule`] and validates its protocol versions.
    pub async fn read_validated_schedule(
        &self,
        reader: &AlloyUpgradeSignalReader,
        log_context: &'static str,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let schedule = self.read_schedule(reader, log_context, metrics_layers).await?;
        self.validate_schedule_protocol_versions(&schedule)?;

        Ok(schedule)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use base_common_genesis::BaseUpgrade;
    use rstest::rstest;

    use super::*;
    use crate::state::{UpgradeSignal, UpgradeSignalSchedule};

    fn upgrade(upgrade_id: &str) -> BaseUpgrade {
        BaseUpgrade::from_contract_fork_name(upgrade_id).unwrap()
    }

    fn supported_config() -> UpgradeSignalConfig {
        let mut config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        config
    }

    #[test]
    fn defaults_to_finalized_block_tag() {
        let config = UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));

        assert_eq!(config.l1_block_tag, UpgradeSignalBlockTag::Finalized);
        assert_eq!(config.request_timeout, UpgradeSignalDefaults::REQUEST_TIMEOUT);
    }

    fn signal(protocol_version: U256) -> UpgradeSignal {
        UpgradeSignal { upgrade_id: BaseUpgrade::Azul, activation_timestamp: 42, protocol_version }
    }

    #[test]
    fn accepts_signal_at_node_protocol_version() {
        let config = UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));

        assert!(
            config.validate_signal_protocol_version(&signal(config.node_protocol_version)).is_ok()
        );
    }

    #[test]
    fn rejects_signal_above_node_protocol_version() {
        // Node supports 1.1.0; a 1.1.1 minimum is genuinely newer.
        let config = supported_config();
        let minimum_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 1);

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(minimum_protocol_version)).unwrap_err(),
            crate::UpgradeSignalError::UnsupportedProtocolVersion { .. }
        ));
    }

    #[test]
    fn accepts_prerelease_minimum_of_the_node_release() {
        // Node runs the final 1.2.3; a 1.2.3-rc.1 minimum must be considered sufficient, even
        // though its raw packed integer is larger than the release's.
        let mut config = supported_config();
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 2, 3);

        let prerelease = PackedProtocolVersion::pack(1, 2, 3, 1).into_inner();
        assert!(prerelease > config.node_protocol_version);
        assert!(config.validate_signal_protocol_version(&signal(prerelease)).is_ok());
    }

    #[test]
    fn rejects_prerelease_minimum_above_the_node_release() {
        // A 1.2.4-rc.1 minimum still outranks the node's final 1.2.3.
        let mut config = supported_config();
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 2, 3);

        let prerelease = PackedProtocolVersion::pack(1, 2, 4, 1).into_inner();
        assert!(matches!(
            config.validate_signal_protocol_version(&signal(prerelease)).unwrap_err(),
            crate::UpgradeSignalError::UnsupportedProtocolVersion { .. }
        ));
    }

    #[test]
    fn rejects_signal_with_unrecognized_version_type() {
        // A non-zero version-type is a format the node cannot interpret. Its semver fields here are
        // all zero, so ignoring the version-type would decode it as `0.0.0` and wrongly accept it
        // (fail-open) under the node's 1.1.0; the version-type must instead rank it above the node
        // so it is rejected (fail-closed).
        let config = supported_config();
        let unknown_version_type = U256::from(1) << 248;

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(unknown_version_type)).unwrap_err(),
            crate::UpgradeSignalError::UnsupportedProtocolVersion { .. }
        ));
    }

    #[test]
    fn rejects_positive_signal_without_protocol_version() {
        let config = UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(U256::ZERO)).unwrap_err(),
            crate::UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }

    fn malformed_schedule(config: &UpgradeSignalConfig) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            vec![
                signal(config.node_protocol_version),
                UpgradeSignal {
                    upgrade_id: BaseUpgrade::Beryl,
                    activation_timestamp: 5,
                    protocol_version: U256::ZERO,
                },
            ],
        )
    }

    #[test]
    fn schedule_validation_rejects_missing_protocol_version() {
        let config = UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));
        let schedule = malformed_schedule(&config);

        assert!(matches!(
            config.validate_schedule_protocol_versions(&schedule).unwrap_err(),
            crate::UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }

    #[test]
    fn schedule_validation_rejects_unsupported_protocol_version() {
        let config = supported_config();

        let schedule = UpgradeSignalSchedule::new(
            1,
            vec![
                UpgradeSignal {
                    upgrade_id: BaseUpgrade::Azul,
                    activation_timestamp: 42,
                    protocol_version: config.node_protocol_version,
                },
                UpgradeSignal {
                    upgrade_id: BaseUpgrade::Beryl,
                    activation_timestamp: 42,
                    protocol_version: UpgradeSignalDefaults::packed_protocol_version(1, 1, 1),
                },
            ],
        );

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
        // Node supports 1.1.0; a 1.1.1 minimum is genuinely unsupported, yet a clear (activation
        // timestamp `0`) must still be allowed regardless of the ordering.
        let config = supported_config();
        let schedule = UpgradeSignalSchedule::new(
            1,
            vec![UpgradeSignal {
                upgrade_id: upgrade(upgrade_id),
                activation_timestamp: 0,
                protocol_version: UpgradeSignalDefaults::packed_protocol_version(1, 1, 1),
            }],
        );

        assert!(config.validate_schedule_protocol_versions(&schedule).is_ok());
    }
}
