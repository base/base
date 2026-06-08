//! Upgrade signal state values.

use std::collections::BTreeMap;

use alloy_primitives::U256;
use base_common_genesis::HardForkConfig;

use crate::UpgradeSignalMetrics;

/// L1 upgrade signal values for one hardfork ID.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UpgradeSignal {
    /// Hardfork ID passed to the L1 contract.
    pub hardfork_id: String,
    /// L2 activation timestamp announced on L1.
    pub activation_timestamp: u64,
    /// Minimum node protocol version announced on L1.
    pub protocol_version: U256,
    /// L1 block number used for the contract read.
    pub l1_block_number: u64,
}

impl UpgradeSignal {
    /// Returns the positive activation timestamp announced for this hardfork.
    pub fn positive_activation_timestamp(&self) -> Option<u64> {
        (self.activation_timestamp > 0).then_some(self.activation_timestamp)
    }

    /// Returns true if both signals contain the same contract-backed upgrade values.
    pub fn has_same_contract_values(&self, other: &Self) -> bool {
        self.hardfork_id == other.hardfork_id
            && self.activation_timestamp == other.activation_timestamp
            && self.protocol_version == other.protocol_version
    }
}

/// L1 upgrade signal values for a configured hardfork schedule.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct UpgradeSignalSchedule {
    /// Signals read from L1.
    pub signals: Vec<UpgradeSignal>,
}

impl UpgradeSignalSchedule {
    /// Creates a new upgrade signal schedule.
    pub fn new(signals: Vec<UpgradeSignal>) -> Self {
        Self { signals }
    }

    /// Returns a copy of this schedule containing only the requested hardfork IDs.
    pub fn filtered_to_hardfork_ids(&self, hardfork_ids: &[String]) -> Self {
        let signals = self
            .signals
            .iter()
            .filter(|signal| {
                hardfork_ids
                    .iter()
                    .any(|hardfork_id| Self::hardfork_ids_match(&signal.hardfork_id, hardfork_id))
            })
            .cloned()
            .collect();

        Self { signals }
    }

    /// Returns true if two hardfork ID spellings refer to the same contract hardfork.
    pub fn hardfork_ids_match(left: &str, right: &str) -> bool {
        match (
            HardForkConfig::canonical_hardfork_id(left),
            HardForkConfig::canonical_hardfork_id(right),
        ) {
            (Some(left), Some(right)) => left == right,
            _ => left.trim().eq_ignore_ascii_case(right.trim()),
        }
    }
}

/// Result of applying a live signal read to local metrics state.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpgradeSignalStateUpdate {
    /// The signal established the initial live baseline.
    Initialized,
    /// The signal is identical to the previous live signal.
    Unchanged,
    /// The signal changed while the node was live.
    Changed,
}

/// Stateful live metrics tracker for one hardfork ID.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct UpgradeSignalState {
    /// Last signal read from L1 by the live metrics observer.
    pub signal: Option<UpgradeSignal>,
}

impl UpgradeSignalState {
    /// Creates an empty upgrade signal state tracker.
    pub const fn new() -> Self {
        Self { signal: None }
    }

    /// Applies a newly read live signal.
    pub fn update_signal(&mut self, signal: UpgradeSignal) -> UpgradeSignalStateUpdate {
        let update = match self.signal.as_ref() {
            Some(previous) if previous.has_same_contract_values(&signal) => {
                UpgradeSignalStateUpdate::Unchanged
            }
            Some(_) => UpgradeSignalStateUpdate::Changed,
            None => UpgradeSignalStateUpdate::Initialized,
        };

        self.signal = Some(signal);
        update
    }
}

/// Records live upgrade signal metrics without mutating node configuration.
#[derive(Debug, Clone)]
pub struct UpgradeSignalMonitor {
    /// Live metrics state by hardfork ID.
    pub states: BTreeMap<String, UpgradeSignalState>,
}

impl UpgradeSignalMonitor {
    /// Creates a monitor for the provided hardfork IDs.
    pub fn new(hardfork_ids: &[String]) -> Self {
        UpgradeSignalMetrics::init();
        let mut states = BTreeMap::new();
        for hardfork_id in hardfork_ids {
            UpgradeSignalMetrics::activation_observed(hardfork_id.clone()).set(0.0);
            states.insert(hardfork_id.clone(), UpgradeSignalState::new());
        }
        Self { states }
    }

    /// Applies signals read from L1 and records corresponding live metrics.
    pub fn update_schedule(
        &mut self,
        schedule: UpgradeSignalSchedule,
    ) -> Vec<UpgradeSignalStateUpdate> {
        schedule.signals.into_iter().map(|signal| self.update_signal(signal)).collect()
    }

    /// Applies one signal read from L1 and records corresponding live metrics.
    pub fn update_signal(&mut self, signal: UpgradeSignal) -> UpgradeSignalStateUpdate {
        let hardfork_id = signal.hardfork_id.clone();
        UpgradeSignalMetrics::record_signal(&signal);

        let update = self.states.entry(hardfork_id.clone()).or_default().update_signal(signal);
        if matches!(update, UpgradeSignalStateUpdate::Changed) {
            UpgradeSignalMetrics::record_signal_update(&hardfork_id);
        }

        update
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    fn signal(timestamp: u64) -> UpgradeSignal {
        UpgradeSignal {
            hardfork_id: "azul".to_string(),
            activation_timestamp: timestamp,
            protocol_version: U256::from(7),
            l1_block_number: 1,
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

    #[test]
    fn l1_block_update_does_not_count_as_contract_value_change() {
        let mut state = UpgradeSignalState::new();
        let mut updated_signal = signal(10);

        state.update_signal(signal(10));
        updated_signal.l1_block_number = 2;

        assert_eq!(state.update_signal(updated_signal), UpgradeSignalStateUpdate::Unchanged);
    }

    #[test]
    fn filters_schedule_by_canonical_hardfork_id() {
        let schedule = UpgradeSignalSchedule::new(vec![
            signal(42),
            UpgradeSignal {
                hardfork_id: "beryl".to_string(),
                activation_timestamp: 43,
                protocol_version: U256::from(7),
                l1_block_number: 1,
            },
        ]);

        let filtered = schedule.filtered_to_hardfork_ids(&["base_azul".to_string()]);

        assert_eq!(filtered.signals.len(), 1);
        assert_eq!(filtered.signals[0].hardfork_id, "azul");
    }
}
