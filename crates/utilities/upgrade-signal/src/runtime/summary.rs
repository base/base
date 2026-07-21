use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::UpgradeSignalSchedule;

/// Runtime action taken for one upgrade signal.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeSignalApplyAction {
    /// The upgrade timestamp was applied.
    Applied,
    /// The upgrade timestamp was cleared.
    Cleared,
    /// The upgrade ID is not supported by this node.
    Ignored,
}

/// Runtime application result for one upgrade signal.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpgradeSignalApplyChange {
    /// Upgrade ID read from the L1 contract.
    pub upgrade_id: String,
    /// Action taken for the upgrade ID.
    pub action: UpgradeSignalApplyAction,
    /// Activation timestamp read from the L1 contract.
    pub activation_timestamp: u64,
    /// Minimum node protocol version read from the L1 contract.
    pub minimum_protocol_version: String,
    /// L1 block number used for the contract read.
    pub l1_block_number: u64,
}

/// Runtime application summary for an upgrade signal schedule.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpgradeSignalApplySummary {
    /// L2 chain ID whose runtime upgrade view was updated.
    pub chain_id: u64,
    /// L1 block number used for the contract read.
    pub l1_block_number: Option<u64>,
    /// Number of configured upgrade signals read from L1.
    pub configured_upgrades: usize,
    /// Number of upgrade timestamps applied.
    pub applied_upgrades: usize,
    /// Number of upgrade timestamps cleared.
    pub cleared_upgrades: usize,
    /// Number of unsupported upgrade signals ignored.
    pub ignored_upgrades: usize,
    /// Per-upgrade application results.
    pub changes: Vec<UpgradeSignalApplyChange>,
}

impl UpgradeSignalApplySummary {
    /// Creates an empty runtime application summary.
    pub fn new(chain_id: u64, schedule: &UpgradeSignalSchedule) -> Self {
        Self {
            chain_id,
            l1_block_number: schedule.signals.iter().map(|signal| signal.l1_block_number).max(),
            configured_upgrades: schedule.signals.len(),
            applied_upgrades: 0,
            cleared_upgrades: 0,
            ignored_upgrades: 0,
            changes: Vec::new(),
        }
    }

    /// Logs each per-upgrade action and a summary line for an applied schedule.
    ///
    /// `target` names the destination the schedule was applied to (e.g. "rollup config").
    pub fn log(&self, target: &'static str) {
        for change in &self.changes {
            match change.action {
                UpgradeSignalApplyAction::Applied => info!(
                    target: "upgrade_signal",
                    destination = target,
                    upgrade_id = %change.upgrade_id,
                    activation_timestamp = change.activation_timestamp,
                    "applied upgrade signal"
                ),
                UpgradeSignalApplyAction::Cleared => info!(
                    target: "upgrade_signal",
                    destination = target,
                    upgrade_id = %change.upgrade_id,
                    "cleared upgrade signal"
                ),
                UpgradeSignalApplyAction::Ignored => debug!(
                    target: "upgrade_signal",
                    destination = target,
                    upgrade_id = %change.upgrade_id,
                    activation_timestamp = change.activation_timestamp,
                    "ignored unsupported upgrade signal"
                ),
            }
        }
        info!(
            target: "upgrade_signal",
            destination = target,
            chain_id = self.chain_id,
            l1_block_number = ?self.l1_block_number,
            applied_upgrades = self.applied_upgrades,
            cleared_upgrades = self.cleared_upgrades,
            ignored_upgrades = self.ignored_upgrades,
            configured_upgrades = self.configured_upgrades,
            "applied upgrade signal schedule"
        );
    }
}
