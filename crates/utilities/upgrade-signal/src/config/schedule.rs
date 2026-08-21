use core::time::Duration;

use alloy_primitives::{Address, U256};
use backon::{ConstantBuilder, Retryable};
use base_common_genesis::UpgradeActivationSink;
use tracing::{error, info};
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

/// What a node should do with a startup schedule after the fail-closed policy has been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupScheduleAction {
    /// Apply the schedule to the node's configuration.
    Apply,
    /// Do not apply the schedule; the node is starting with a loud alarm because it cannot apply an
    /// upgrade that is still far from activation (a live poller will fail it closed later).
    Skip,
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

        // Render both versions as semver (not the raw >70-digit packed decimals) so the error
        // message states the exact node-vs-contract gap an operator must close.
        Err(UpgradeSignalError::unsupported_protocol_version(
            signal.upgrade_id.contract_id().to_string(),
            PackedProtocolVersion::new(signal.protocol_version),
            PackedProtocolVersion::new(self.node_protocol_version),
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

    /// Lead time before an unsupportable upgrade's activation at which the node fails closed.
    ///
    /// Derived from the configured L1 block tag's poll cadence
    /// ([`UpgradeSignalDefaults::HALT_LEAD_POLL_INTERVALS`] × the tag's poll interval) rather than a
    /// fixed constant, so the halt reliably lands one or more polls *before* activation whether the
    /// node reads `latest`, `safe`, or `finalized`. See
    /// [`UpgradeSignalDefaults::HALT_LEAD_POLL_INTERVALS`] for why the window is kept short.
    pub const fn halt_lead_time(&self) -> Duration {
        Duration::from_secs(
            self.l1_block_tag.poll_interval().as_secs()
                * UpgradeSignalDefaults::HALT_LEAD_POLL_INTERVALS as u64,
        )
    }

    /// Returns the first signal that requires the node to fail closed, if any.
    ///
    /// This is a signal whose activation is positive (an activation, not a clear), whose minimum
    /// protocol version this node is too old to support, and whose activation is at most
    /// `lead_secs` seconds after `now_secs` (or already past). Such an upgrade will fork the network
    /// at activation and this node cannot follow it, so the node must halt before then.
    ///
    /// A malformed signal (a positive activation with a zero minimum version) is deliberately *not*
    /// returned here: a zero version is trivially supported by the version check, so it is surfaced
    /// as a non-fatal alarm elsewhere rather than halting the node on an L1 misconfiguration.
    pub fn fail_closed_upgrade<'a>(
        &self,
        schedule: &'a UpgradeSignalSchedule,
        now_secs: u64,
        lead_secs: u64,
    ) -> Option<&'a UpgradeSignal> {
        schedule.signals.iter().find(|signal| {
            self.validate_signal_supported_protocol_version(signal).is_err()
                && now_secs.saturating_add(lead_secs) >= signal.activation_timestamp
        })
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
        let Some(schedule) = self
            .read_startup_schedule(
                &reader,
                log_context,
                &[UpgradeSignalMetricLayer::Execution, UpgradeSignalMetricLayer::Consensus],
                UpgradeSignalDefaults::STARTUP_SCHEDULE_RETRY_INTERVAL,
            )
            .await?
        else {
            return Ok(());
        };

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

    /// Reads the startup schedule and applies the fail-closed policy, returning whether it should be
    /// applied.
    ///
    /// Two fail-closed behaviors combine here:
    ///
    /// * **Empty or unreachable contract** — a healthy append-only `ProtocolVersions` contract is
    ///   never empty, so an empty read (like a transient provider failure) means the node cannot yet
    ///   see the authoritative activation schedule. Booting on the genesis/base configuration would
    ///   risk activating forks at different times than peers that read a populated contract, forking
    ///   the node — and any blocks it builds — off the network. Rather than take that risk, the read
    ///   retries without an attempt limit, logging loudly at `error!` on every failure, until the
    ///   contract returns a non-empty schedule. Only unrecoverable errors that waiting cannot fix —
    ///   malformed contract data ([`UpgradeSignalError::Decode`]) — propagate and abort startup.
    ///   This future is cancellation-safe: dropping it during shutdown cancels the in-flight request
    ///   or retry sleep.
    /// * **Unsupportable upgrade** — once a non-empty schedule is read, the lead-time fail-closed
    ///   policy is applied so a restart is not blocked by an upgrade that is still far off: see
    ///   [`Self::evaluate_startup_schedule`]. Returns `None` when the schedule must be skipped
    ///   (started with an alarm), or an error that must abort startup.
    ///
    /// `retry_interval` is the fixed delay between empty/provider retries; it is paced for legible
    /// retry logs rather than fast recovery (see
    /// [`UpgradeSignalDefaults::STARTUP_SCHEDULE_RETRY_INTERVAL`]).
    pub async fn read_startup_schedule(
        &self,
        reader: &AlloyUpgradeSignalReader,
        log_context: &'static str,
        metrics_layers: &[UpgradeSignalMetricLayer],
        retry_interval: Duration,
    ) -> Result<Option<UpgradeSignalSchedule>, UpgradeSignalError> {
        let mut attempt = 1_u64;
        let backoff = ConstantBuilder::default().with_delay(retry_interval).without_max_times();

        // Retry only the raw read until the contract is non-empty and reachable. Validation and the
        // lead-time policy run once on the resulting schedule (below), so a distant unsupportable
        // upgrade is deferred to the policy rather than retried forever.
        let schedule = (|| self.read_schedule(reader, log_context, metrics_layers))
            .retry(backoff)
            .when(|error| {
                matches!(
                    error,
                    UpgradeSignalError::EmptySchedule | UpgradeSignalError::Provider { .. }
                )
            })
            .notify(|error, retry_delay| {
                error!(
                    target: "upgrade_signal",
                    context = log_context,
                    attempt,
                    retry_delay_ms = u64::try_from(retry_delay.as_millis()).unwrap_or(u64::MAX),
                    error = %error,
                    "refusing to start without an authoritative L1 upgrade schedule; retrying"
                );
                attempt += 1;
            })
            .await?;

        match self.evaluate_startup_schedule(
            &schedule,
            UpgradeSignalDefaults::now_secs(),
            self.halt_lead_time().as_secs(),
            metrics_layers,
        )? {
            StartupScheduleAction::Apply => Ok(Some(schedule)),
            StartupScheduleAction::Skip => Ok(None),
        }
    }

    /// Applies the startup fail-closed policy to an already-read schedule.
    ///
    /// This is the startup analogue of the runtime poller's fail-closed handling, so a node that
    /// applies at startup behaves consistently with one that polls live:
    ///
    /// * If an unsupportable upgrade activates within `lead_secs` of `now_secs` (or is overdue), the
    ///   node must not start only to fork at activation — this returns an error that aborts startup.
    /// * Otherwise, if the schedule fully validates, it is applied.
    /// * If validation fails but nothing is imminent (a far-future unsupportable upgrade or a
    ///   malformed L1 signal), the handling depends on whether a live poller will back-stop it:
    ///   [`UpgradeSignalMode::RuntimeAdmin`] starts with a loud alarm and skips applying (the poller
    ///   will fail closed once the upgrade nears activation), while a mode without a live poller
    ///   stays strict and aborts startup, since nothing would catch the upgrade later.
    pub fn evaluate_startup_schedule(
        &self,
        schedule: &UpgradeSignalSchedule,
        now_secs: u64,
        lead_secs: u64,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<StartupScheduleAction, UpgradeSignalError> {
        if let Some(signal) = self.fail_closed_upgrade(schedule, now_secs, lead_secs) {
            for layer in metrics_layers {
                UpgradeSignalMetrics::record_fail_closed(*layer, signal);
            }
            error!(
                target: "upgrade_signal",
                upgrade = %signal.upgrade_id.contract_id(),
                activation_timestamp = signal.activation_timestamp,
                node_protocol_version = %PackedProtocolVersion::new(self.node_protocol_version),
                minimum_protocol_version = %PackedProtocolVersion::new(signal.protocol_version),
                "refusing to start (fail closed): a scheduled L1 upgrade activates within the halt lead time but this node's protocol version is too old to apply it; upgrade this node to a supported version"
            );
            return Err(UpgradeSignalError::NodeUpgradeRequired {
                upgrade_id: signal.upgrade_id.contract_id().to_string(),
                activation_timestamp: signal.activation_timestamp,
                minimum_protocol_version: PackedProtocolVersion::new(signal.protocol_version)
                    .to_string(),
                node_protocol_version: PackedProtocolVersion::new(self.node_protocol_version)
                    .to_string(),
            });
        }

        let Err(validation_error) = self.validate_schedule_protocol_versions(schedule) else {
            return Ok(StartupScheduleAction::Apply);
        };

        // Validation failed but nothing is imminent. Only a mode with a live poller can safely start
        // and defer the halt; other modes stay strict.
        if !self.mode.allows_runtime_admin() {
            return Err(validation_error);
        }
        error!(
            target: "upgrade_signal",
            node_protocol_version = %PackedProtocolVersion::new(self.node_protocol_version),
            contract_protocol_versions = %schedule.required_protocol_versions(),
            error = %validation_error,
            "starting despite an L1 upgrade schedule that cannot be applied locally; upgrade this node before the upgrade nears activation or it will fail closed and stop"
        );
        Ok(StartupScheduleAction::Skip)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use base_common_genesis::BaseUpgrade;
    use rstest::rstest;

    use super::*;
    use crate::{
        state::{UpgradeSignal, UpgradeSignalSchedule},
        test_utils::MockL1,
    };

    fn upgrade(upgrade_id: &str) -> BaseUpgrade {
        BaseUpgrade::from_contract_fork_name(upgrade_id).unwrap()
    }

    fn supported_config() -> UpgradeSignalConfig {
        let mut config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        config
    }

    /// ABI-encodes a `uint64[]` with a single zeroed entry (offset, length, one word). A zero
    /// activation needs no protocol version, so the read passes validation without one.
    fn single_zeroed_entry() -> Vec<u8> {
        let mut single_entry = vec![0_u8; 96];
        single_entry[31] = 32;
        single_entry[63] = 1;
        single_entry
    }

    /// ABI-encodes an empty `uint64[]` (offset word, zero length).
    fn empty_schedule() -> Vec<u8> {
        let mut empty = vec![0_u8; 64];
        empty[31] = 32;
        empty
    }

    #[tokio::test]
    async fn startup_read_returns_present_schedule() {
        let server = MockL1::schedule_server(single_zeroed_entry()).await;
        let config = UpgradeSignalConfig::new(Address::ZERO);
        let reader = config.reader(server.url("/").parse().unwrap()).unwrap();

        let schedule = config
            .read_startup_schedule(
                &reader,
                "startup",
                &[UpgradeSignalMetricLayer::Consensus],
                Duration::from_millis(10),
            )
            .await
            .unwrap()
            .expect("a supported schedule is applied");

        assert_eq!(schedule.signals.len(), 1);
        assert_eq!(schedule.signals[0].upgrade_id, BaseUpgrade::Regolith);
        assert_eq!(schedule.signals[0].activation_timestamp, 0);
    }

    #[tokio::test]
    async fn startup_read_retries_until_schedule_is_present() {
        // The contract reports an empty schedule at first. Fail-closed startup must not boot on the
        // base configuration; it retries until the contract returns a real schedule, then applies
        // it. The mock is swapped from empty to populated once the empty read is observed.
        let server = MockL1::block_and_min_protocol_server().await;
        let empty_mock = MockL1::mock_get_schedule(&server, empty_schedule()).await;
        let config = UpgradeSignalConfig::new(Address::ZERO);
        let reader = config.reader(server.url("/").parse().unwrap()).unwrap();

        let swap = async {
            while empty_mock.calls_async().await == 0 {
                tokio::task::yield_now().await;
            }
            empty_mock.delete_async().await;
            MockL1::mock_get_schedule(&server, single_zeroed_entry()).await;
        };
        let (schedule, ()) = tokio::join!(
            config.read_startup_schedule(
                &reader,
                "startup",
                &[UpgradeSignalMetricLayer::Consensus],
                Duration::from_millis(10),
            ),
            swap,
        );

        let schedule = schedule.unwrap().expect("a supported schedule is applied");
        assert_eq!(schedule.signals.len(), 1);
        assert_eq!(schedule.signals[0].upgrade_id, BaseUpgrade::Regolith);
    }

    #[tokio::test]
    async fn startup_read_aborts_on_fatal_error() {
        // A `getSchedule` that returns malformed data (`0x`) is a decode error: waiting cannot fix
        // it, so fail-closed startup surfaces it immediately instead of looping forever.
        let server = MockL1::block_and_min_protocol_server().await;
        MockL1::mock_get_schedule(&server, Vec::new()).await;
        let config = UpgradeSignalConfig::new(Address::ZERO);
        let reader = config.reader(server.url("/").parse().unwrap()).unwrap();

        let error = config
            .read_startup_schedule(
                &reader,
                "startup",
                &[UpgradeSignalMetricLayer::Consensus],
                Duration::from_millis(10),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, UpgradeSignalError::Decode { .. }));
    }

    #[test]
    fn defaults_to_finalized_block_tag() {
        let config = UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"));

        assert_eq!(config.l1_block_tag, UpgradeSignalBlockTag::Finalized);
        assert_eq!(config.request_timeout, UpgradeSignalDefaults::REQUEST_TIMEOUT);
    }

    #[rstest]
    #[case(UpgradeSignalBlockTag::Finalized)]
    #[case(UpgradeSignalBlockTag::Safe)]
    #[case(UpgradeSignalBlockTag::Latest)]
    fn halt_lead_time_is_a_small_multiple_of_the_poll_interval(#[case] tag: UpgradeSignalBlockTag) {
        // The halt window is derived from the read cadence (never a fixed 24h) so it lands one or
        // more polls before activation regardless of the block tag, and is short enough not to
        // front-run the outage.
        let mut config = supported_config();
        config.l1_block_tag = tag;

        assert_eq!(
            config.halt_lead_time(),
            tag.poll_interval() * UpgradeSignalDefaults::HALT_LEAD_POLL_INTERVALS
        );
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

    const STARTUP_LAYERS: &[UpgradeSignalMetricLayer] = &[UpgradeSignalMetricLayer::Consensus];

    fn config_with_mode(mode: UpgradeSignalMode) -> UpgradeSignalConfig {
        let mut config = supported_config();
        config.mode = mode;
        config
    }

    fn schedule_at(activation_timestamp: u64, protocol_version: U256) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            vec![UpgradeSignal {
                upgrade_id: BaseUpgrade::Azul,
                activation_timestamp,
                protocol_version,
            }],
        )
    }

    #[test]
    fn startup_applies_a_supported_schedule() {
        let config = config_with_mode(UpgradeSignalMode::RuntimeAdmin);
        let lead = config.halt_lead_time().as_secs();
        let schedule =
            schedule_at(1_000_000, UpgradeSignalDefaults::packed_protocol_version(1, 1, 0));

        assert_eq!(
            config.evaluate_startup_schedule(&schedule, 0, lead, STARTUP_LAYERS).unwrap(),
            StartupScheduleAction::Apply
        );
    }

    #[test]
    fn startup_aborts_when_an_unsupportable_upgrade_is_imminent() {
        let config = config_with_mode(UpgradeSignalMode::RuntimeAdmin);
        let lead = config.halt_lead_time().as_secs();
        let activation = 1_000_000;
        let schedule =
            schedule_at(activation, UpgradeSignalDefaults::packed_protocol_version(1, 1, 1));

        assert!(matches!(
            config.evaluate_startup_schedule(
                &schedule,
                activation - lead + 1,
                lead,
                STARTUP_LAYERS
            ),
            Err(UpgradeSignalError::NodeUpgradeRequired { .. })
        ));
    }

    #[test]
    fn startup_skips_a_distant_unsupportable_upgrade_when_a_live_poller_will_backstop_it() {
        let config = config_with_mode(UpgradeSignalMode::RuntimeAdmin);
        let lead = config.halt_lead_time().as_secs();
        let schedule =
            schedule_at(u64::MAX, UpgradeSignalDefaults::packed_protocol_version(1, 1, 1));

        assert_eq!(
            config.evaluate_startup_schedule(&schedule, 0, lead, STARTUP_LAYERS).unwrap(),
            StartupScheduleAction::Skip
        );
    }

    #[test]
    fn startup_stays_strict_for_a_distant_unsupportable_upgrade_without_a_live_poller() {
        let config = config_with_mode(UpgradeSignalMode::StartupApply);
        let lead = config.halt_lead_time().as_secs();
        let schedule =
            schedule_at(u64::MAX, UpgradeSignalDefaults::packed_protocol_version(1, 1, 1));

        assert!(config.evaluate_startup_schedule(&schedule, 0, lead, STARTUP_LAYERS).is_err());
    }

    #[test]
    fn startup_skips_a_malformed_signal_in_runtime_admin_and_never_fails_closed_on_it() {
        let config = config_with_mode(UpgradeSignalMode::RuntimeAdmin);
        let lead = config.halt_lead_time().as_secs();
        // A malformed signal (positive activation, no minimum version), even long overdue.
        let schedule = schedule_at(1, U256::ZERO);

        assert_eq!(
            config.evaluate_startup_schedule(&schedule, u64::MAX, lead, STARTUP_LAYERS).unwrap(),
            StartupScheduleAction::Skip
        );
    }
}
