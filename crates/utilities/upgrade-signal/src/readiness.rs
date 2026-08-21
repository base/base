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

use crate::{PackedProtocolVersion, UpgradeSignal, UpgradeSignalConfig};

/// A node's readiness for the currently scheduled contract-backed upgrades.
///
/// `ready` is the single go/no-go answer an operator (or a fleet-wide rollout script) checks before
/// the upgrade is scheduled on L1. The remaining fields carry the raw inputs so the result is
/// self-explanatory without a second query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeReadiness {
    /// Whether this node is ready.
    ///
    /// When a [`target`](Self::target) version was supplied, this is simply whether the node
    /// supports that version. Otherwise it is whether the node supports every upgrade currently
    /// scheduled on L1 (vacuously `true` when nothing is scheduled — so a `false` here is always an
    /// actionable "this node needs upgrading", never "nothing to check").
    pub ready: bool,
    /// This node's advertised protocol version, rendered as `major.minor.patch` semver.
    pub node_protocol_version: String,
    /// L1 block number the schedule was read at, or `None` when the contract has no schedule yet.
    pub l1_block_number: Option<u64>,
    /// Result of comparing the node against a caller-supplied target version, when one was given.
    ///
    /// Operators use this to verify support for an *announced* upgrade before it is scheduled on L1,
    /// when the contract does not yet carry the new minimum. Absent when no target was supplied.
    pub target: Option<UpgradeReadinessTarget>,
    /// Per-upgrade readiness for every activation currently scheduled on L1 (cleared/unscheduled
    /// entries are omitted).
    pub upgrades: Vec<UpgradeReadinessEntry>,
}

/// Readiness against a caller-supplied target protocol version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeReadinessTarget {
    /// The requested target minimum, rendered as `major.minor.patch` semver.
    pub required_protocol_version: String,
    /// Whether this node's advertised version satisfies the target.
    pub supported: bool,
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
    /// Whether the node would fail closed (halt) for this upgrade right now: it is unsupported *and*
    /// the activation is within the halt lead window (or already past). This tracks the node's real
    /// halt decision exactly, so it only becomes `true` as an unsupported upgrade nears activation.
    pub would_halt: bool,
}

impl UpgradeSignalConfig {
    /// Builds an [`UpgradeReadiness`] report for `signals` (as read from L1 at `l1_block_number`).
    ///
    /// `now_secs` is the wall-clock used for the halt-window ([`UpgradeReadinessEntry::would_halt`])
    /// check. `target_version`, when `Some`, overrides the on-chain minimum for the top-level
    /// [`UpgradeReadiness::ready`] answer: it lets an operator confirm support for an *announced*
    /// upgrade before it is scheduled on L1 (the gap between rolling out a release and publishing the
    /// schedule), when the contract does not yet carry the new minimum. When `None`, readiness is
    /// judged against the upgrades currently scheduled on L1.
    ///
    /// Only signals with a positive activation timestamp are reported (a clear carries no meaningful
    /// minimum), and every field is computed with the same predicates the live poller uses so the
    /// report cannot diverge from the node's actual behavior.
    pub fn evaluate_readiness(
        &self,
        signals: &[UpgradeSignal],
        l1_block_number: Option<u64>,
        now_secs: u64,
        target_version: Option<U256>,
    ) -> UpgradeReadiness {
        let lead_secs = self.halt_lead_time().as_secs();

        let upgrades: Vec<UpgradeReadinessEntry> = signals
            .iter()
            .filter(|signal| signal.activation_timestamp > 0)
            .map(|signal| UpgradeReadinessEntry {
                upgrade_id: signal.upgrade_id.contract_id().to_string(),
                required_protocol_version: PackedProtocolVersion::new(signal.protocol_version)
                    .to_string(),
                activation_timestamp: signal.activation_timestamp,
                supported: self.supports_signal_protocol_version(signal),
                would_halt: self.signal_fails_closed(signal, now_secs, lead_secs),
            })
            .collect();

        let target = target_version.map(|version| UpgradeReadinessTarget {
            required_protocol_version: PackedProtocolVersion::new(version).to_string(),
            supported: self.supports_protocol_version(version),
        });

        // A supplied target is the operator's explicit "am I ready for the announced upgrade?"
        // question and takes precedence; otherwise fall back to the upgrades currently on L1.
        let ready = target.as_ref().map_or_else(
            || upgrades.iter().all(|upgrade| upgrade.supported),
            |target| target.supported,
        );

        UpgradeReadiness {
            ready,
            node_protocol_version: PackedProtocolVersion::new(self.node_protocol_version)
                .to_string(),
            l1_block_number,
            target,
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

    fn config() -> UpgradeSignalConfig {
        let mut config = UpgradeSignalConfig::new(Address::ZERO);
        // Node advertises 1.1.0.
        config.node_protocol_version = UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        config
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
        assert_eq!(readiness.node_protocol_version, "1.1.0");
        assert_eq!(readiness.l1_block_number, Some(42));
        assert_eq!(readiness.upgrades.len(), 1);
        let entry = &readiness.upgrades[0];
        assert_eq!(entry.upgrade_id, "azul");
        assert_eq!(entry.required_protocol_version, "1.1.0");
        assert!(entry.supported);
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
    fn target_version_overrides_the_ready_answer_before_anything_is_scheduled() {
        let config = config();

        // Empty contract (pre-schedule): a supported target is ready.
        let supported_target = UpgradeSignalDefaults::packed_protocol_version(1, 0, 0);
        let ready = config.evaluate_readiness(&[], None, 0, Some(supported_target));
        assert!(ready.ready);
        assert_eq!(ready.l1_block_number, None);
        assert!(ready.upgrades.is_empty());
        let target = ready.target.expect("target reported");
        assert_eq!(target.required_protocol_version, "1.0.0");
        assert!(target.supported);

        // An unsupported target is not ready, even with nothing scheduled.
        let unsupported_target = UpgradeSignalDefaults::packed_protocol_version(2, 0, 0);
        let not_ready = config.evaluate_readiness(&[], None, 0, Some(unsupported_target));
        assert!(!not_ready.ready);
        assert!(!not_ready.target.unwrap().supported);
    }
}
