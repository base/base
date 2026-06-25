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
    /// The hardfork ID is not supported by this node.
    Ignored,
}

/// Runtime application result for one upgrade signal.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpgradeSignalApplyChange {
    /// Hardfork ID read from the L1 contract.
    pub hardfork_id: String,
    /// Action taken for the hardfork ID.
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
    /// L2 chain ID whose runtime fork view was updated.
    pub chain_id: u64,
    /// L1 block number used for the contract read.
    pub l1_block_number: Option<u64>,
    /// Number of configured hardfork signals read from L1.
    pub configured_hardforks: usize,
    /// Number of hardfork timestamps applied.
    pub applied_hardforks: usize,
    /// Number of hardfork timestamps cleared.
    pub cleared_hardforks: usize,
    /// Number of unsupported hardfork signals ignored.
    pub ignored_hardforks: usize,
    /// Per-hardfork application results.
    pub changes: Vec<UpgradeSignalApplyChange>,
}

impl UpgradeSignalApplySummary {
    /// Creates an empty runtime application summary.
    pub fn new(chain_id: u64, schedule: &UpgradeSignalSchedule) -> Self {
        Self {
            chain_id,
            l1_block_number: schedule.signals.iter().map(|signal| signal.l1_block_number).max(),
            configured_hardforks: schedule.signals.len(),
            applied_hardforks: 0,
            cleared_hardforks: 0,
            ignored_hardforks: 0,
            changes: Vec::new(),
        }
    }

    /// Logs each per-hardfork action and a summary line for an applied schedule.
    ///
    /// `target` names the destination the schedule was applied to (e.g. "rollup config").
    pub fn log(&self, target: &'static str) {
        for change in &self.changes {
            match change.action {
                UpgradeSignalApplyAction::Applied => info!(
                    target: "upgrade_signal",
                    destination = target,
                    hardfork_id = %change.hardfork_id,
                    activation_timestamp = change.activation_timestamp,
                    "applied upgrade signal"
                ),
                UpgradeSignalApplyAction::Cleared => info!(
                    target: "upgrade_signal",
                    destination = target,
                    hardfork_id = %change.hardfork_id,
                    "cleared upgrade signal"
                ),
                UpgradeSignalApplyAction::Ignored => debug!(
                    target: "upgrade_signal",
                    destination = target,
                    hardfork_id = %change.hardfork_id,
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
            applied_hardforks = self.applied_hardforks,
            cleared_hardforks = self.cleared_hardforks,
            ignored_hardforks = self.ignored_hardforks,
            configured_hardforks = self.configured_hardforks,
            "applied upgrade signal schedule"
        );
    }
}
