//! Upgrade signal state values.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::U256;
use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry};
use tracing::{debug, error, info};

use crate::{
    AlloyUpgradeSignalReader, PackedProtocolVersion, UpgradeSignalDefaults, UpgradeSignalError,
    UpgradeSignalMetricLayer, UpgradeSignalMetrics, UpgradeSignalRefresher,
};

/// L1 upgrade signal values for one contract-backed upgrade.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UpgradeSignal {
    /// Contract-backed upgrade passed to the L1 contract.
    pub upgrade_id: BaseUpgrade,
    /// L2 activation timestamp announced on L1.
    pub activation_timestamp: u64,
    /// Minimum node protocol version announced on L1.
    pub protocol_version: U256,
}

impl UpgradeSignal {
    /// Returns the positive activation timestamp announced for this upgrade.
    pub fn positive_activation_timestamp(&self) -> Option<u64> {
        (self.activation_timestamp > 0).then_some(self.activation_timestamp)
    }

    /// Returns true if both signals contain the same contract-backed upgrade values.
    pub fn has_same_contract_values(&self, other: &Self) -> bool {
        self.upgrade_id == other.upgrade_id
            && self.activation_timestamp == other.activation_timestamp
            && self.protocol_version == other.protocol_version
    }
}

/// L1 upgrade signal values for a configured upgrade schedule.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UpgradeSignalSchedule {
    /// L1 block number used to read the complete schedule.
    pub l1_block_number: u64,
    /// Signals read from L1.
    pub signals: Vec<UpgradeSignal>,
}

impl UpgradeSignalSchedule {
    /// Creates a new upgrade signal schedule.
    pub const fn new(l1_block_number: u64, signals: Vec<UpgradeSignal>) -> Self {
        Self { l1_block_number, signals }
    }

    /// Renders the minimum node protocol version each active signal demands as space-separated
    /// `upgrade=version` pairs, using the semver display of the packed contract value.
    ///
    /// Only signals with a positive activation timestamp carry a meaningful minimum (a clear
    /// carries none), so cleared signals are omitted. Used to log the exact node-vs-contract
    /// version gap when a live apply fails, so it reads directly rather than as packed decimals.
    pub fn required_protocol_versions(&self) -> String {
        self.signals
            .iter()
            .filter(|signal| signal.activation_timestamp > 0)
            .map(|signal| {
                format!(
                    "{}={}",
                    signal.upgrade_id.contract_id(),
                    PackedProtocolVersion::new(signal.protocol_version)
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Result of applying a live signal read to local metrics state.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum UpgradeSignalStateUpdate {
    /// The signal established the initial live baseline.
    Initialized,
    /// The signal is identical to the previous live signal.
    Unchanged,
    /// The signal changed while the node was live.
    Changed,
}

impl UpgradeSignalStateUpdate {
    /// Returns true when this update requires re-applying the schedule.
    ///
    /// [`Self::Initialized`] requires apply: a signal observed live for the first time may carry
    /// a schedule change that landed after the baseline should have been established (an upgrade
    /// registered on L1 after node start, or a startup window of failed reads), so it must not be
    /// silently adopted as the baseline.
    const fn requires_apply(self) -> bool {
        matches!(self, Self::Initialized | Self::Changed)
    }
}

/// Stateful live tracker for one contract-backed upgrade.
///
/// Two independent baselines are tracked: `observed` advances on every read and drives metrics and
/// change detection, while `applied` advances only when a schedule is successfully committed to the
/// runtime registry. Keeping them separate means a failed apply does not poison the baseline: the
/// signal keeps being offered for apply until a commit actually succeeds.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct UpgradeSignalState {
    /// Last signal read from L1 by the live observer.
    observed: Option<UpgradeSignal>,
    /// Last signal a successful apply committed to the runtime registry.
    applied: Option<UpgradeSignal>,
}

impl UpgradeSignalState {
    /// Creates an empty upgrade signal state tracker.
    const fn new() -> Self {
        Self { observed: None, applied: None }
    }

    /// Records a newly read live signal against the observed baseline.
    fn update_signal(&mut self, signal: UpgradeSignal) -> UpgradeSignalStateUpdate {
        let update = match self.observed.as_ref() {
            Some(previous) if previous.has_same_contract_values(&signal) => {
                UpgradeSignalStateUpdate::Unchanged
            }
            Some(_) => UpgradeSignalStateUpdate::Changed,
            None => UpgradeSignalStateUpdate::Initialized,
        };

        self.observed = Some(signal);
        update
    }

    /// Returns true when `signal` has not yet been successfully applied.
    fn needs_apply(&self, signal: &UpgradeSignal) -> bool {
        self.applied.as_ref().is_none_or(|applied| !applied.has_same_contract_values(signal))
    }

    /// Advances the applied baseline after a successful commit.
    fn mark_applied(&mut self, signal: &UpgradeSignal) {
        self.applied = Some(signal.clone());
    }

    /// Clears the applied baseline after this upgrade is reconciled out of the schedule.
    const fn clear_applied(&mut self) {
        self.applied = None;
    }
}

/// Outcome of a single live poll, telling the caller whether the node must fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a HaltNode outcome must fail the node closed to avoid forking off the network"]
pub enum UpgradeSignalPollOutcome {
    /// The node may keep running.
    Continue,
    /// A scheduled upgrade this node is too old to support activates within the halt lead time; the
    /// node must fail closed (stop) rather than continue and fork off the network at activation.
    ///
    /// Carries the offending upgrade so the caller can surface a self-contained fatal error without
    /// re-reading the log.
    HaltNode {
        /// Upgrade whose imminent activation forces the halt.
        upgrade_id: BaseUpgrade,
        /// L2 activation timestamp of that upgrade.
        activation_timestamp: u64,
        /// Minimum node protocol version the upgrade requires (packed).
        minimum_protocol_version: U256,
        /// Node protocol version this binary advertises (packed).
        node_protocol_version: U256,
    },
}

/// Records live upgrade signal metrics and, when a refresher is supplied, auto-applies observed
/// schedule changes.
#[derive(Debug, Clone)]
pub struct UpgradeSignalMonitor {
    /// Metric layer recorded by this monitor.
    pub metrics_layer: UpgradeSignalMetricLayer,
    /// Live metrics and apply state by contract-backed upgrade.
    states: BTreeMap<BaseUpgrade, UpgradeSignalState>,
    /// Contract values of the last schedule that failed to apply, used to page only on the first
    /// occurrence of a persistent failure rather than on every poll.
    last_apply_failure: Option<Vec<UpgradeSignal>>,
}

impl UpgradeSignalMonitor {
    /// Creates a monitor for all contract-backed upgrades.
    pub fn new(metrics_layer: UpgradeSignalMetricLayer) -> Self {
        UpgradeSignalMetrics::init();
        let mut states = BTreeMap::new();
        for upgrade_id in BaseUpgrade::CONTRACT_VARIANTS {
            states.insert(upgrade_id, UpgradeSignalState::new());
        }
        Self { metrics_layer, states, last_apply_failure: None }
    }

    /// Tolerantly polls the reader, records live metrics, and — when `refresher` is supplied —
    /// applies any schedule not yet successfully committed.
    ///
    /// This is the single live-poll routine shared by the consensus actor and the execution
    /// metrics extension. Read failures are recorded but do not abort the poll and do not advance
    /// either baseline; a schedule that reads cleanly but cannot be applied is handled by
    /// [`Self::apply_and_evaluate`], which may return [`UpgradeSignalPollOutcome::HaltNode`]. The
    /// caller MUST fail the node closed on `HaltNode`.
    pub async fn poll_and_apply(
        &mut self,
        reader: &AlloyUpgradeSignalReader,
        refresher: Option<&UpgradeSignalRefresher>,
    ) -> UpgradeSignalPollOutcome {
        let Some(schedule) = reader.read_schedule_tolerant(&[self.metrics_layer]).await else {
            return UpgradeSignalPollOutcome::Continue;
        };

        let observed_changes = self
            .update_schedule(schedule.clone())
            .iter()
            .filter(|update| update.requires_apply())
            .count();
        if observed_changes > 0 {
            info!(
                target: "upgrade_signal",
                updated_signals = observed_changes,
                "observed live L1 upgrade signal update"
            );
        }

        let Some(refresher) = refresher else {
            return UpgradeSignalPollOutcome::Continue;
        };
        if !self.schedule_needs_apply(refresher.chain_id, &schedule) {
            return UpgradeSignalPollOutcome::Continue;
        }

        self.apply_and_evaluate(refresher, &schedule, UpgradeSignalDefaults::now_secs())
    }

    /// Applies a schedule to the runtime registry, or decides the node must fail closed.
    ///
    /// An apply performs no I/O and cannot flake: the L1 read is a separate, earlier step (whose
    /// failures return before this point) and the runtime registry write is infallible, so every
    /// failure is a deterministic local validation error — either an outdated node (the schedule
    /// requires a newer protocol version than this binary advertises) or a malformed L1 signal (a
    /// scheduled activation with no minimum protocol version). There is therefore nothing to retry.
    ///
    /// A stale schedule can also be *rejected* rather than failed: the registry refuses a schedule
    /// read from an older L1 block than the one it already committed, returning `Ok` with
    /// `committed == false`. That is not an error, so it never halts; the applied baseline is left
    /// untouched exactly as on failure, so a newer-block re-read of the same schedule still applies.
    ///
    /// On failure the applied baseline is deliberately *not* advanced, so the schedule is re-offered
    /// (and re-evaluated) on every subsequent poll. Handling depends on the cause:
    ///
    /// * **Outdated node** — the node cannot follow the upgrade and will fork off the network at its
    ///   activation, so this is fail-closed. While the activation is more than the halt lead time
    ///   (see [`crate::UpgradeSignalConfig::halt_lead_time`]) away the poller only alarms loudly (giving
    ///   the operator time to upgrade); once the activation is within that window (or overdue) it
    ///   returns [`UpgradeSignalPollOutcome::HaltNode`] and the node stops. This escalates
    ///   automatically: a future-dated upgrade that is ignored becomes fatal as its activation
    ///   nears.
    /// * **Malformed L1 signal** — an L1/governance misconfiguration, not a node-version problem, so
    ///   the node alarms loudly but never halts (halting every node on a governance typo would be a
    ///   self-inflicted outage).
    ///
    /// Failures raise the sticky `apply_failed` gauge and increment `apply_failures_total`; the
    /// first occurrence of a distinct failure pages at `error`, later re-observations drop to
    /// `debug` so a weeks-away upgrade does not spam the log every poll.
    fn apply_and_evaluate(
        &mut self,
        refresher: &UpgradeSignalRefresher,
        schedule: &UpgradeSignalSchedule,
        now_secs: u64,
    ) -> UpgradeSignalPollOutcome {
        let apply_error = match refresher.apply(schedule) {
            Ok(summary) => {
                // The registry rejects a schedule read from an older L1 block than the one it has
                // already committed (a lower-block reorg on a non-finalized tag), returning `Ok`
                // with `committed == false` and leaving the registry untouched. Advance the applied
                // baseline only when the registry actually committed the schedule; otherwise the
                // monitor would desync from the registry — recording a spurious success and moving
                // (or clearing) a baseline whose override is still live — and never retry the
                // reconcile once a newer block re-offers the same schedule.
                if summary.committed {
                    self.mark_schedule_applied(schedule);
                    self.last_apply_failure = None;
                    UpgradeSignalMetrics::record_apply_success(self.metrics_layer, schedule);
                }
                return UpgradeSignalPollOutcome::Continue;
            }
            Err(apply_error) => apply_error,
        };

        UpgradeSignalMetrics::record_apply_failure(self.metrics_layer, schedule);
        let first_occurrence =
            self.last_apply_failure.as_deref() != Some(schedule.signals.as_slice());
        if first_occurrence {
            self.last_apply_failure = Some(schedule.signals.clone());
        }

        // Both sides of the protocol-version comparison, logged as semver so an operator reads the
        // exact gap straight from the line: `node_protocol_version` is what this binary advertises,
        // `contract_protocol_versions` is what each active upgrade on L1 demands.
        let node_protocol_version =
            PackedProtocolVersion::new(refresher.config.node_protocol_version);
        let contract_protocol_versions = schedule.required_protocol_versions();

        // Fail closed only for an outdated node whose unsupportable upgrade activates within the
        // lead time. A malformed signal never reaches here (its zero version is trivially supported
        // by the check), so `fail_closed_upgrade` returning `Some` implies the outdated-node cause.
        if let Some(signal) = refresher.config.fail_closed_upgrade(
            schedule,
            now_secs,
            refresher.config.halt_lead_time().as_secs(),
        ) {
            UpgradeSignalMetrics::record_fail_closed(self.metrics_layer, signal);
            error!(
                target: "upgrade_signal",
                upgrade = %signal.upgrade_id.contract_id(),
                activation_timestamp = signal.activation_timestamp,
                node_protocol_version = %node_protocol_version,
                contract_protocol_versions = %contract_protocol_versions,
                error = %apply_error,
                "halting node (fail closed): a scheduled L1 upgrade activates within the halt lead time but this node's protocol version is too old to apply it; the node would fork off the network at activation, so it is stopping. Upgrade this node to a supported version"
            );
            return UpgradeSignalPollOutcome::HaltNode {
                upgrade_id: signal.upgrade_id,
                activation_timestamp: signal.activation_timestamp,
                minimum_protocol_version: signal.protocol_version,
                node_protocol_version: refresher.config.node_protocol_version,
            };
        }

        // Non-fatal: an outdated node whose upgrade is still far off, or a malformed L1 signal.
        // Alarm loudly on the first occurrence, then quieten so a weeks-away upgrade does not spam.
        let node_outdated =
            matches!(&apply_error, UpgradeSignalError::UnsupportedProtocolVersion { .. });
        match (node_outdated, first_occurrence) {
            (true, true) => error!(
                target: "upgrade_signal",
                node_protocol_version = %node_protocol_version,
                contract_protocol_versions = %contract_protocol_versions,
                error = %apply_error,
                "an L1 upgrade schedule cannot be applied locally because this node's protocol version is outdated; upgrade this node before the upgrade activates or it will fail closed and stop"
            ),
            (true, false) => debug!(
                target: "upgrade_signal",
                node_protocol_version = %node_protocol_version,
                contract_protocol_versions = %contract_protocol_versions,
                error = %apply_error,
                "an L1 upgrade schedule still cannot be applied locally because this node's protocol version is outdated; upgrade this node before the upgrade activates or it will fail closed and stop"
            ),
            (false, true) => error!(
                target: "upgrade_signal",
                node_protocol_version = %node_protocol_version,
                contract_protocol_versions = %contract_protocol_versions,
                error = %apply_error,
                "an L1 upgrade schedule cannot be applied locally because the L1 signal is malformed (a scheduled activation carries no minimum protocol version); fix the L1 upgrade signal"
            ),
            (false, false) => debug!(
                target: "upgrade_signal",
                node_protocol_version = %node_protocol_version,
                contract_protocol_versions = %contract_protocol_versions,
                error = %apply_error,
                "an L1 upgrade schedule still cannot be applied locally because the L1 signal is malformed (a scheduled activation carries no minimum protocol version); fix the L1 upgrade signal"
            ),
        }

        UpgradeSignalPollOutcome::Continue
    }

    /// Returns true when the schedule diverges from what is committed to the runtime registry,
    /// either because a present signal has not yet been applied or because an upgrade the registry
    /// still overrides has vanished from a later, shorter schedule and must be reconciled out.
    ///
    /// The removal case covers a pure schedule shrink — for example an L1 reorg on a non-finalized
    /// tag that unwinds a trailing upgrade's registration. Without it the shrink would never trip
    /// the apply gate (the present entries are unchanged), so the stale activation would linger in
    /// the registry until a restart or manual refresh. A governance clear keeps its slot with a
    /// `0` timestamp, so it stays present and is not treated as a removal.
    ///
    /// Removal is detected against the authoritative runtime registry (`chain_id`), not the
    /// monitor's own applied baseline: the admin refresh path writes the registry without touching
    /// `states`, so a registry override this poller never applied itself would otherwise be missed
    /// and its stale activation left in place.
    fn schedule_needs_apply(&self, chain_id: u64, schedule: &UpgradeSignalSchedule) -> bool {
        let present: BTreeSet<BaseUpgrade> =
            schedule.signals.iter().map(|signal| signal.upgrade_id).collect();
        let removed = RuntimeUpgradeRegistry::overrides(chain_id).is_some_and(|overrides| {
            overrides.activations.keys().any(|upgrade_id| !present.contains(upgrade_id))
        });

        removed
            || schedule.signals.iter().any(|signal| {
                self.states.get(&signal.upgrade_id).is_none_or(|state| state.needs_apply(signal))
            })
    }

    /// Reconciles the applied baseline with a successfully committed schedule.
    ///
    /// Advances the baseline for every present signal and clears the baseline of any upgrade absent
    /// from the schedule, mirroring the runtime registry that `replace_overrides` has just trimmed
    /// to exactly this schedule. Clearing the vanished entries keeps the apply gate from re-firing
    /// on every subsequent poll.
    fn mark_schedule_applied(&mut self, schedule: &UpgradeSignalSchedule) {
        let present: BTreeSet<BaseUpgrade> =
            schedule.signals.iter().map(|signal| signal.upgrade_id).collect();
        for (upgrade_id, state) in &mut self.states {
            if !present.contains(upgrade_id) {
                state.clear_applied();
            }
        }
        for signal in &schedule.signals {
            self.states.entry(signal.upgrade_id).or_default().mark_applied(signal);
        }
    }

    /// Applies signals read from L1 and records corresponding live metrics.
    fn update_schedule(
        &mut self,
        schedule: UpgradeSignalSchedule,
    ) -> Vec<UpgradeSignalStateUpdate> {
        schedule
            .signals
            .into_iter()
            .map(|signal| self.update_signal(schedule.l1_block_number, signal))
            .collect()
    }

    /// Applies one signal read from L1 and records corresponding live metrics.
    fn update_signal(
        &mut self,
        l1_block_number: u64,
        signal: UpgradeSignal,
    ) -> UpgradeSignalStateUpdate {
        let upgrade_id = signal.upgrade_id;
        UpgradeSignalMetrics::record_signal(self.metrics_layer, l1_block_number, &signal);

        let update = self.states.entry(upgrade_id).or_default().update_signal(signal);
        if matches!(update, UpgradeSignalStateUpdate::Changed) {
            UpgradeSignalMetrics::record_signal_update(self.metrics_layer, upgrade_id);
        }

        update
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_common_genesis::{RuntimeUpgradeRegistry, UpgradeActivation};

    use super::*;
    use crate::UpgradeSignalConfig;

    fn signal(timestamp: u64) -> UpgradeSignal {
        UpgradeSignal {
            upgrade_id: BaseUpgrade::Azul,
            activation_timestamp: timestamp,
            protocol_version: U256::from(7),
        }
    }

    #[test]
    fn signal_returns_positive_activation_timestamp() {
        assert_eq!(signal(10).positive_activation_timestamp(), Some(10));
    }

    #[test]
    fn signal_ignores_zero_activation_timestamp() {
        assert_eq!(signal(0).positive_activation_timestamp(), None);
    }

    #[test]
    fn state_initializes_then_tracks_unchanged_signal() {
        let mut state = UpgradeSignalState::new();

        assert_eq!(state.update_signal(signal(10)), UpgradeSignalStateUpdate::Initialized);
        assert_eq!(state.update_signal(signal(10)), UpgradeSignalStateUpdate::Unchanged);
    }

    #[test]
    fn state_detects_contract_value_changes() {
        let mut state = UpgradeSignalState::new();

        state.update_signal(signal(10));

        assert_eq!(state.update_signal(signal(12)), UpgradeSignalStateUpdate::Changed);
    }

    fn monitor() -> UpgradeSignalMonitor {
        UpgradeSignalMonitor::new(UpgradeSignalMetricLayer::Consensus)
    }

    /// A refresher whose apply succeeds only when `node_version` satisfies the schedule minimum.
    fn refresher(chain_id: u64, node_version: U256) -> UpgradeSignalRefresher {
        let mut config = UpgradeSignalConfig::new(Address::ZERO);
        config.node_protocol_version = node_version;
        let reader = config.reader("http://127.0.0.1:1".parse().unwrap()).unwrap();
        UpgradeSignalRefresher::new(config, reader, chain_id, UpgradeSignalMetricLayer::Consensus)
    }

    fn versioned_schedule(timestamp: u64, protocol_version: U256) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            vec![UpgradeSignal {
                upgrade_id: BaseUpgrade::Azul,
                activation_timestamp: timestamp,
                protocol_version,
            }],
        )
    }

    fn schedule(timestamp: u64) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(1, vec![signal(timestamp)])
    }

    #[test]
    fn first_observation_and_change_require_apply_but_unchanged_does_not() {
        assert!(UpgradeSignalStateUpdate::Initialized.requires_apply());
        assert!(UpgradeSignalStateUpdate::Changed.requires_apply());
        assert!(!UpgradeSignalStateUpdate::Unchanged.requires_apply());
    }

    #[test]
    fn monitor_counts_first_observation_as_update() {
        let mut monitor = monitor();

        let updates = monitor.update_schedule(schedule(10));

        assert_eq!(updates, vec![UpgradeSignalStateUpdate::Initialized]);
    }

    #[test]
    fn monitor_ignores_unchanged_signal() {
        let mut monitor = monitor();

        monitor.update_schedule(schedule(10));

        assert_eq!(
            monitor.update_schedule(schedule(10)),
            vec![UpgradeSignalStateUpdate::Unchanged]
        );
    }

    #[test]
    fn monitor_ignores_l1_block_update_with_unchanged_contract_values() {
        let mut monitor = monitor();

        monitor.update_schedule(schedule(10));
        let updated_schedule = UpgradeSignalSchedule::new(2, vec![signal(10)]);

        assert_eq!(
            monitor.update_schedule(updated_schedule),
            vec![UpgradeSignalStateUpdate::Unchanged]
        );
    }

    #[test]
    fn monitor_detects_changed_signal() {
        let mut monitor = monitor();

        monitor.update_schedule(schedule(10));

        assert_eq!(monitor.update_schedule(schedule(12)), vec![UpgradeSignalStateUpdate::Changed]);
    }

    #[test]
    fn state_needs_apply_until_marked_applied() {
        let mut state = UpgradeSignalState::new();
        let signal = signal(10);

        // Never applied: needs apply even before it has been observed.
        assert!(state.needs_apply(&signal));

        // Observing the signal advances the observed baseline but not the applied baseline.
        state.update_signal(signal.clone());
        assert!(state.needs_apply(&signal));

        // Only a successful apply advances the applied baseline.
        state.mark_applied(&signal);
        assert!(!state.needs_apply(&signal));
    }

    #[test]
    fn state_changed_signal_needs_apply_after_previous_applied() {
        let mut state = UpgradeSignalState::new();

        state.mark_applied(&signal(10));

        assert!(!state.needs_apply(&signal(10)));
        assert!(state.needs_apply(&signal(12)));
    }

    #[test]
    fn failed_apply_keeps_schedule_offered_for_retry() {
        let chain_id = 9_100_030;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut monitor = monitor();
        let schedule = schedule(10);

        // Mirror `poll_and_apply`: observe first (advances the observed baseline), then a failed
        // apply must NOT advance the applied baseline, so the schedule is offered again next poll.
        monitor.update_schedule(schedule.clone());
        assert!(
            monitor.schedule_needs_apply(chain_id, &schedule),
            "an unapplied schedule must remain offered for retry"
        );

        // A subsequent successful apply advances the applied baseline and stops the retries.
        monitor.mark_schedule_applied(&schedule);
        assert!(!monitor.schedule_needs_apply(chain_id, &schedule));
    }

    #[test]
    fn l1_change_after_failed_apply_is_offered() {
        let chain_id = 9_100_031;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut monitor = monitor();

        // Observe v1 and leave it unapplied (apply failed).
        monitor.update_schedule(schedule(10));
        assert!(monitor.schedule_needs_apply(chain_id, &schedule(10)));

        // L1 then changes to v2, which must still be offered for apply.
        monitor.update_schedule(schedule(12));
        assert!(monitor.schedule_needs_apply(chain_id, &schedule(12)));
    }

    #[test]
    fn applied_schedule_is_not_reoffered() {
        let chain_id = 9_100_032;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut monitor = monitor();
        let schedule = schedule(10);

        monitor.update_schedule(schedule.clone());
        monitor.mark_schedule_applied(&schedule);

        // The same contract values, even at a new L1 block number, are not re-offered.
        let same_values_new_block = UpgradeSignalSchedule::new(2, vec![signal(10)]);
        assert!(!monitor.schedule_needs_apply(chain_id, &same_values_new_block));
    }

    #[test]
    fn required_protocol_versions_renders_active_signals_as_semver() {
        let schedule = UpgradeSignalSchedule::new(
            1,
            vec![
                UpgradeSignal {
                    upgrade_id: BaseUpgrade::Azul,
                    activation_timestamp: 42,
                    protocol_version: UpgradeSignalDefaults::packed_protocol_version(1, 2, 3),
                },
                // A cleared signal (activation 0) carries no meaningful minimum and is omitted.
                UpgradeSignal {
                    upgrade_id: BaseUpgrade::Beryl,
                    activation_timestamp: 0,
                    protocol_version: UpgradeSignalDefaults::packed_protocol_version(9, 9, 9),
                },
            ],
        );

        assert_eq!(schedule.required_protocol_versions(), "azul=1.2.3");
    }

    // A far-future activation the fail-closed check will never consider imminent.
    const FAR_FUTURE: u64 = u64::MAX;

    #[test]
    fn apply_and_evaluate_commits_a_supported_schedule() {
        let chain_id = 9_100_010;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        // A dev-build node version (the sentinel maximum) satisfies any minimum, so the apply
        // succeeds and the node continues.
        let refresher = refresher(chain_id, UpgradeSignalDefaults::node_protocol_version());
        let schedule =
            versioned_schedule(42, UpgradeSignalDefaults::packed_protocol_version(1, 1, 0));

        let mut monitor = monitor();
        monitor.update_schedule(schedule.clone());
        let outcome = monitor.apply_and_evaluate(&refresher, &schedule, 0);

        assert_eq!(outcome, UpgradeSignalPollOutcome::Continue);
        assert!(!monitor.schedule_needs_apply(chain_id, &schedule));
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn schedule_shrink_reconciles_removed_upgrade_out_of_registry() {
        let chain_id = 9_100_050;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let refresher = refresher(chain_id, UpgradeSignalDefaults::node_protocol_version());
        let version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let upgrade = |upgrade_id, activation_timestamp| UpgradeSignal {
            upgrade_id,
            activation_timestamp,
            protocol_version: version,
        };

        // Apply a two-upgrade schedule; both land in the runtime registry.
        let full = UpgradeSignalSchedule::new(
            1,
            vec![upgrade(BaseUpgrade::Azul, 42), upgrade(BaseUpgrade::Beryl, 84)],
        );
        let mut monitor = monitor();
        monitor.update_schedule(full.clone());
        assert_eq!(
            monitor.apply_and_evaluate(&refresher, &full, 0),
            UpgradeSignalPollOutcome::Continue
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl),
            Some(UpgradeActivation::Timestamp(84))
        );

        // A later, shorter schedule (read at a newer L1 block) drops Beryl entirely while the
        // remaining Azul entry is unchanged — the pure-shrink case from an L1 reorg.
        let shrunk = UpgradeSignalSchedule::new(2, vec![upgrade(BaseUpgrade::Azul, 42)]);
        monitor.update_schedule(shrunk.clone());
        assert!(
            monitor.schedule_needs_apply(chain_id, &shrunk),
            "a vanished upgrade must trigger reconciliation even when present entries are unchanged"
        );

        // Applying the shrunk schedule trims the removed upgrade out of the registry.
        assert_eq!(
            monitor.apply_and_evaluate(&refresher, &shrunk, 0),
            UpgradeSignalPollOutcome::Continue
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl), None);

        // The baseline is reconciled too, so the shrunk schedule is not re-offered next poll.
        assert!(
            !monitor.schedule_needs_apply(chain_id, &shrunk),
            "a reconciled shrink must not re-fire the apply gate"
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn rejected_lower_block_schedule_does_not_advance_the_baseline() {
        let chain_id = 9_100_060;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let refresher = refresher(chain_id, UpgradeSignalDefaults::node_protocol_version());
        let version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let azul = |activation_timestamp| UpgradeSignal {
            upgrade_id: BaseUpgrade::Azul,
            activation_timestamp,
            protocol_version: version,
        };

        // Commit Azul@42 at L1 block 100; it lands in the registry and advances the baseline.
        let committed = UpgradeSignalSchedule::new(100, vec![azul(42)]);
        let mut monitor = monitor();
        monitor.update_schedule(committed.clone());
        assert_eq!(
            monitor.apply_and_evaluate(&refresher, &committed, 0),
            UpgradeSignalPollOutcome::Continue
        );

        // A reorg on a non-finalized tag re-reads Azul@50 at the older L1 block 99. The registry
        // rejects the stale schedule (`Ok` with `committed == false`) and keeps Azul@42.
        let stale = UpgradeSignalSchedule::new(99, vec![azul(50)]);
        monitor.update_schedule(stale.clone());
        assert_eq!(
            monitor.apply_and_evaluate(&refresher, &stale, 0),
            UpgradeSignalPollOutcome::Continue
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        // Because the rejected apply left the baseline untouched, Azul@50 is still offered so a
        // newer-block re-read can commit it; the baseline was not desynced to the uncommitted value.
        assert!(
            monitor.schedule_needs_apply(chain_id, &stale),
            "a rejected stale schedule must not advance the applied baseline"
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn removal_is_detected_against_registry_written_by_admin_refresh() {
        let chain_id = 9_100_070;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let refresher = refresher(chain_id, UpgradeSignalDefaults::node_protocol_version());
        let version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let upgrade = |upgrade_id, activation_timestamp| UpgradeSignal {
            upgrade_id,
            activation_timestamp,
            protocol_version: version,
        };

        // The admin refresh commits {Azul, Beryl} straight to the registry without touching this
        // monitor's in-memory state — exactly the drift the registry-based removal check guards.
        let admin = UpgradeSignalSchedule::new(
            1,
            vec![upgrade(BaseUpgrade::Azul, 42), upgrade(BaseUpgrade::Beryl, 84)],
        );
        assert!(refresher.apply(&admin).unwrap().committed);

        // The monitor's own baseline knows only Azul (its present entry is already applied), so a
        // baseline-only removal check would miss Beryl. The registry still overrides Beryl, so the
        // shrink must trip the apply gate anyway.
        let mut monitor = monitor();
        monitor.mark_schedule_applied(&UpgradeSignalSchedule::new(
            1,
            vec![upgrade(BaseUpgrade::Azul, 42)],
        ));
        let shrunk = UpgradeSignalSchedule::new(2, vec![upgrade(BaseUpgrade::Azul, 42)]);
        monitor.update_schedule(shrunk.clone());
        assert!(
            monitor.schedule_needs_apply(chain_id, &shrunk),
            "a registry override this monitor never applied itself must still trip the apply gate"
        );

        // Applying the shrunk schedule trims Beryl out of the registry.
        assert_eq!(
            monitor.apply_and_evaluate(&refresher, &shrunk, 0),
            UpgradeSignalPollOutcome::Continue
        );
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl), None);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn apply_and_evaluate_alarms_but_continues_for_a_distant_unsupportable_upgrade() {
        let chain_id = 9_100_011;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        // Node supports 1.1.0; the schedule demands 1.1.1 and activates far in the future.
        let node_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let refresher = refresher(chain_id, node_version);
        let schedule =
            versioned_schedule(FAR_FUTURE, UpgradeSignalDefaults::packed_protocol_version(1, 1, 1));

        let mut monitor = monitor();
        monitor.update_schedule(schedule.clone());
        let outcome = monitor.apply_and_evaluate(&refresher, &schedule, 0);

        // Not yet fatal: the node keeps running, nothing is committed, and the schedule stays
        // offered (baseline not advanced) so a later poll re-evaluates it.
        assert_eq!(outcome, UpgradeSignalPollOutcome::Continue);
        assert!(monitor.schedule_needs_apply(chain_id, &schedule));
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn apply_and_evaluate_halts_when_unsupportable_upgrade_is_within_lead_time() {
        let chain_id = 9_100_012;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let node_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let refresher = refresher(chain_id, node_version);
        let activation = 1_000_000;
        let schedule =
            versioned_schedule(activation, UpgradeSignalDefaults::packed_protocol_version(1, 1, 1));

        let mut monitor = monitor();
        monitor.update_schedule(schedule.clone());
        // "Now" is inside the halt lead time before activation.
        let now = activation - refresher.config.halt_lead_time().as_secs() + 1;
        let outcome = monitor.apply_and_evaluate(&refresher, &schedule, now);

        assert!(matches!(
            outcome,
            UpgradeSignalPollOutcome::HaltNode { upgrade_id: BaseUpgrade::Azul, .. }
        ));
        assert_eq!(RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul), None);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn apply_and_evaluate_alarms_but_never_halts_on_a_malformed_signal() {
        let chain_id = 9_100_013;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        // A malformed signal: a positive activation (already overdue) with no minimum version.
        let refresher = refresher(chain_id, UpgradeSignalDefaults::node_protocol_version());
        let schedule = versioned_schedule(1, U256::ZERO);

        let mut monitor = monitor();
        monitor.update_schedule(schedule.clone());
        // Even with "now" well past the activation, a malformed signal never fails the node closed.
        let outcome = monitor.apply_and_evaluate(&refresher, &schedule, u64::MAX);

        assert_eq!(outcome, UpgradeSignalPollOutcome::Continue);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn fail_closed_upgrade_only_flags_imminent_unsupportable_activations() {
        let mut config = UpgradeSignalConfig::new(Address::ZERO);
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        let lead = config.halt_lead_time().as_secs();
        let activation = 1_000_000;
        let unsupported =
            versioned_schedule(activation, UpgradeSignalDefaults::packed_protocol_version(1, 1, 1));

        // Outside the lead window: not flagged.
        assert!(config.fail_closed_upgrade(&unsupported, activation - lead - 1, lead).is_none());
        // Inside the lead window: flagged.
        assert!(config.fail_closed_upgrade(&unsupported, activation - lead + 1, lead).is_some());
        // Overdue: flagged.
        assert!(config.fail_closed_upgrade(&unsupported, activation + 100, lead).is_some());

        // A supported upgrade is never flagged, even when overdue.
        let supported =
            versioned_schedule(activation, UpgradeSignalDefaults::packed_protocol_version(1, 1, 0));
        assert!(config.fail_closed_upgrade(&supported, activation + 100, lead).is_none());
    }
}
