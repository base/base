//! Upgrade readiness reporting for operator pre-flight checks.
//!
//! Between rolling a release out to node operators and scheduling the upgrade on L1, operators need
//! a way to confirm their node will follow the upgrade rather than fall behind (or, once the
//! fail-closed poller is active, halt) at activation. [`UpgradeReadiness`] is the machine-readable
//! answer returned by the public `base_upgradeReadiness` RPC (and rendered by the `basectl
//! upgrade-readiness` subcommand).
//!
//! The report is deliberately built from the *same* predicates the live node uses —
//! [`UpgradeSignalConfig::supports_signal_protocol_version`] and
//! [`UpgradeSignalConfig::signal_fails_closed`] — so it can never disagree with what the node
//! actually does at activation.

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::{PackedProtocolVersion, UpgradeSignal, UpgradeSignalConfig, UpgradeSignalMode};

/// A node's readiness for the currently scheduled contract-backed upgrades.
///
/// `ready` is the single go/no-go answer an operator (or a fleet-wide rollout script) checks before
/// the upgrade is scheduled on L1. The remaining fields carry the raw inputs so the result is
/// self-explanatory without a second query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeReadiness {
    /// Whether this node is ready.
    ///
    /// When a caller-supplied target version was supplied, this is a pure binary-support check: does
    /// this node's version satisfy the target, independent of mode.
    ///
    /// Otherwise it reflects whether the node will actually follow the upgrades currently scheduled
    /// on L1, which requires both that the node's [`mode`](Self::mode) applies the live schedule
    /// *and* that the schedule passes the same validation the apply path runs (every activation has
    /// a supported, well-formed minimum version). A `false` here is therefore always actionable —
    /// see [`reason`](Self::reason) — never "nothing to check" (an empty schedule is vacuously
    /// ready for an applying mode).
    pub ready: bool,
    /// This node's advertised protocol version, rendered as `major.minor.patch` semver.
    pub node_protocol_version: String,
    /// This node's upgrade-signal mode, which determines whether it applies the live L1 schedule.
    ///
    /// Only [`UpgradeSignalMode::RuntimeAdmin`] tracks and applies the schedule after startup, so
    /// for the other modes the on-chain schedule below is reported for visibility but the node will
    /// not follow a live change to it (reflected in `ready`).
    pub mode: UpgradeSignalMode,
    /// When `ready` is `false`, a human-readable explanation: an unsupported node version, a
    /// malformed on-chain schedule, or a mode that does not apply the live schedule. `None` when
    /// ready.
    pub reason: Option<String>,
    /// L1 block number the schedule was read at, or `None` when the contract has no schedule yet.
    pub l1_block_number: Option<u64>,
    /// Per-upgrade readiness for every activation currently scheduled on L1 (cleared/unscheduled
    /// entries are omitted).
    pub upgrades: Vec<UpgradeReadinessEntry>,
}

/// A node's readiness for one scheduled contract-backed upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeReadinessEntry {
    /// Contract upgrade identifier (e.g. `azul`).
    pub upgrade_id: String,
    /// Minimum node protocol version this upgrade requires on L1, rendered as `major.minor.patch`.
    pub required_protocol_version: String,
    /// L2 activation timestamp announced on L1.
    pub activation_timestamp: u64,
    /// Whether this node's advertised version satisfies the required minimum.
    ///
    /// This is the pure version comparison, independent of the activation timing, so it stays
    /// meaningful the instant a minimum is published — even before an activation timestamp is set.
    pub supported: bool,
    /// Whether this scheduled signal is malformed: it has an activation timestamp but no minimum
    /// protocol version (see [`UpgradeSignalConfig::validate_signal_has_protocol_version`]).
    ///
    /// The node would refuse to apply such a schedule, so a malformed entry makes the overall report
    /// unready even though its zero minimum version compares as trivially `supported`.
    pub malformed: bool,
    /// Whether the node would fail closed (halt) for this upgrade right now: its mode applies the
    /// live schedule, it is unsupported, *and* the activation is within the halt lead window (or
    /// already past). This tracks the node's real halt decision exactly — so it stays `false` in a
    /// mode that never halts live, and otherwise only becomes `true` as an unsupported upgrade nears
    /// activation.
    pub would_halt: bool,
}

impl UpgradeSignalConfig {
    /// Builds an [`UpgradeReadiness`] report for `signals` (as read from L1 at `l1_block_number`).
    ///
    /// `now_secs` is the wall-clock used for the halt-window ([`UpgradeReadinessEntry::would_halt`])
    /// check. `target_version`, when `Some`, overrides the on-chain minimum for the top-level
    /// [`UpgradeReadiness::ready`] answer with a pure binary-support check: it lets an operator
    /// confirm support for an *announced* upgrade before it is scheduled on L1 (the gap between
    /// rolling out a release and publishing the schedule), independent of this node's mode. When
    /// `None`, readiness is judged against the upgrades currently scheduled on L1.
    ///
    /// For that on-chain path, `ready` requires both that this node's [mode](UpgradeSignalConfig)
    /// applies the live schedule and that the schedule passes the same validation the apply path
    /// runs ([`Self::validate_signal_protocol_version`]) — so the report can never claim the node
    /// will follow a schedule it would refuse to apply, or that a non-applying mode will follow one
    /// at all. Only signals with a positive activation timestamp are itemized (a clear carries no
    /// meaningful minimum).
    pub fn evaluate_readiness(
        &self,
        signals: &[UpgradeSignal],
        l1_block_number: Option<u64>,
        now_secs: u64,
        target_version: Option<U256>,
    ) -> UpgradeReadiness {
        let lead_secs = self.halt_lead_time().as_secs();
        // Only a live-applying mode actually follows a live schedule change or halts at activation;
        // in the other modes the node observes the schedule but never applies or halts on it, so the
        // report must not claim it will.
        let applies_live = self.mode.applies_live_schedule();

        let upgrades: Vec<UpgradeReadinessEntry> = signals
            .iter()
            .filter(|signal| signal.activation_timestamp > 0)
            .map(|signal| UpgradeReadinessEntry {
                upgrade_id: signal.upgrade_id.contract_id().to_string(),
                required_protocol_version: PackedProtocolVersion::new(signal.protocol_version)
                    .to_string(),
                activation_timestamp: signal.activation_timestamp,
                supported: self.supports_signal_protocol_version(signal),
                malformed: self.validate_signal_has_protocol_version(signal).is_err(),
                would_halt: applies_live && self.signal_fails_closed(signal, now_secs, lead_secs),
            })
            .collect();

        let (ready, reason) = match target_version {
            // A supplied target is the operator's explicit "does my binary support the announced
            // version?" probe used to gate a rollout before the upgrade is scheduled on L1. It is a
            // pure binary-capability check, independent of this node's apply mode.
            Some(version) if self.supports_protocol_version(version) => (true, None),
            Some(version) => (
                false,
                Some(format!(
                    "node protocol version {} does not support target {}",
                    PackedProtocolVersion::new(self.node_protocol_version),
                    PackedProtocolVersion::new(version),
                )),
            ),
            // Otherwise judge against the upgrades currently scheduled on L1: the node must be in a
            // mode that applies the live schedule and pass the same validation the apply path runs.
            None if !applies_live => (
                false,
                Some(
                    "node's upgrade-signal mode does not apply the live L1 schedule; it will not \
                     follow a live change to it (per-upgrade `supported` still reflects binary \
                     capability)"
                        .to_string(),
                ),
            ),
            None => signals
                .iter()
                .find_map(|signal| self.validate_signal_protocol_version(signal).err())
                .map_or((true, None), |error| (false, Some(error.to_string()))),
        };

        UpgradeReadiness {
            ready,
            node_protocol_version: PackedProtocolVersion::new(self.node_protocol_version)
                .to_string(),
            mode: self.mode,
            reason,
            l1_block_number,
            upgrades,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_common_genesis::BaseUpgrade;

    use super::*;
    use crate::UpgradeSignalDefaults;

    fn config_with_mode(mode: UpgradeSignalMode) -> UpgradeSignalConfig {
        let mut config = UpgradeSignalConfig::new(Address::ZERO);
        // Node advertises 1.1.0.
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        config.mode = mode;
        config
    }

    /// A live-applying node, for which the on-chain readiness verdict is meaningful.
    fn config() -> UpgradeSignalConfig {
        config_with_mode(UpgradeSignalMode::RuntimeAdmin)
    }

    fn signal(upgrade_id: BaseUpgrade, activation_timestamp: u64, version: U256) -> UpgradeSignal {
        UpgradeSignal { upgrade_id, activation_timestamp, protocol_version: version }
    }

    #[test]
    fn reports_supported_scheduled_upgrade_as_ready() {
        let config = config();
        let signals = [signal(
            BaseUpgrade::Azul,
            1_000,
            UpgradeSignalDefaults::packed_protocol_version(1, 1, 0),
        )];

        let readiness = config.evaluate_readiness(&signals, Some(42), 0, None);

        assert!(readiness.ready);
        assert!(readiness.reason.is_none());
        assert_eq!(readiness.mode, UpgradeSignalMode::RuntimeAdmin);
        assert_eq!(readiness.node_protocol_version, "1.1.0");
        assert_eq!(readiness.l1_block_number, Some(42));
        assert_eq!(readiness.upgrades.len(), 1);
        let entry = &readiness.upgrades[0];
        assert_eq!(entry.upgrade_id, "azul");
        assert_eq!(entry.required_protocol_version, "1.1.0");
        assert!(entry.supported);
        assert!(!entry.malformed);
        assert!(!entry.would_halt);
    }

    #[test]
    fn unsupported_upgrade_is_not_ready_and_halts_only_when_imminent() {
        let config = config();
        // Requires 1.1.1, which the 1.1.0 node cannot satisfy.
        let required = UpgradeSignalDefaults::packed_protocol_version(1, 1, 1);
        let activation = 1_000_000;

        // Far in the future: not ready, but not yet within the halt window.
        let distant = config.evaluate_readiness(
            &[signal(BaseUpgrade::Azul, activation, required)],
            Some(1),
            0,
            None,
        );
        assert!(!distant.ready);
        assert!(distant.reason.is_some());
        assert!(!distant.upgrades[0].supported);
        assert!(!distant.upgrades[0].would_halt);

        // Within the halt lead window: would halt.
        let now = activation - config.halt_lead_time().as_secs() + 1;
        let imminent = config.evaluate_readiness(
            &[signal(BaseUpgrade::Azul, activation, required)],
            Some(1),
            now,
            None,
        );
        assert!(!imminent.ready);
        assert!(imminent.upgrades[0].would_halt);
    }

    #[test]
    fn cleared_signals_are_omitted() {
        let config = config();
        // Activation 0 is a clear and carries no meaningful minimum, even a high one.
        let signals =
            [signal(BaseUpgrade::Azul, 0, UpgradeSignalDefaults::packed_protocol_version(9, 9, 9))];

        let readiness = config.evaluate_readiness(&signals, Some(1), 0, None);

        assert!(readiness.upgrades.is_empty());
        // Nothing scheduled that the node cannot support, so it is ready.
        assert!(readiness.ready);
    }

    #[test]
    fn malformed_scheduled_signal_is_not_ready() {
        let config = config();
        // A positive activation with a zero minimum version is malformed: the node would refuse to
        // apply this schedule, so readiness must be false even though the zero version compares as
        // trivially supported.
        let signals = [signal(BaseUpgrade::Azul, 1_000, U256::ZERO)];

        let readiness = config.evaluate_readiness(&signals, Some(1), 0, None);

        assert!(!readiness.ready);
        assert!(readiness.reason.is_some());
        let entry = &readiness.upgrades[0];
        assert!(entry.malformed);
        // The zero version trivially compares as supported, which is exactly why `malformed` (not
        // `supported`) is what makes the report unready.
        assert!(entry.supported);
        assert!(!entry.would_halt);
    }

    #[test]
    fn non_applying_mode_is_not_ready_even_when_supported() {
        // A supportable, well-formed schedule that a runtime-admin node would follow.
        let signals = [signal(
            BaseUpgrade::Azul,
            1_000,
            UpgradeSignalDefaults::packed_protocol_version(1, 1, 0),
        )];

        for mode in [UpgradeSignalMode::MetricsOnly, UpgradeSignalMode::StartupApply] {
            let readiness = config_with_mode(mode).evaluate_readiness(&signals, Some(1), 0, None);

            // The node's mode never applies a live schedule change, so it cannot be reported ready
            // for the on-chain schedule, and it never halts live either.
            assert!(!readiness.ready, "{mode:?} should not be ready for the live schedule");
            assert!(readiness.reason.is_some());
            assert_eq!(readiness.mode, mode);
            assert!(readiness.upgrades[0].supported);
            assert!(!readiness.upgrades[0].would_halt);
        }
    }

    #[test]
    fn target_version_overrides_the_ready_answer_before_anything_is_scheduled() {
        let config = config();

        // Empty contract (pre-schedule): a supported target is ready.
        let supported_target = UpgradeSignalDefaults::packed_protocol_version(1, 0, 0);
        let ready = config.evaluate_readiness(&[], None, 0, Some(supported_target));
        assert!(ready.ready);
        assert!(ready.reason.is_none());
        assert_eq!(ready.l1_block_number, None);
        assert!(ready.upgrades.is_empty());

        // An unsupported target is not ready, even with nothing scheduled.
        let unsupported_target = UpgradeSignalDefaults::packed_protocol_version(2, 0, 0);
        let not_ready = config.evaluate_readiness(&[], None, 0, Some(unsupported_target));
        assert!(!not_ready.ready);
        assert!(not_ready.reason.is_some());
    }

    #[test]
    fn target_version_check_is_mode_independent() {
        // The target probe is a pure binary-capability check for gating a rollout, so it answers the
        // same regardless of whether this node's mode applies the live schedule.
        let supported_target = UpgradeSignalDefaults::packed_protocol_version(1, 0, 0);

        for mode in [
            UpgradeSignalMode::MetricsOnly,
            UpgradeSignalMode::StartupApply,
            UpgradeSignalMode::RuntimeAdmin,
        ] {
            let readiness =
                config_with_mode(mode).evaluate_readiness(&[], None, 0, Some(supported_target));
            assert!(readiness.ready, "{mode:?} should support the target version");
        }
    }
}
