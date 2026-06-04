//! Upgrade signal state tracking.

use std::collections::BTreeMap;

use alloy_primitives::U256;
use tracing::{debug, info, warn};

use crate::{UpgradeSignalConfig, UpgradeSignalMetrics};

/// L1 upgrade signal values for one hardfork ID.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UpgradeSignal {
    /// Hardfork ID passed to the L1 contract.
    pub hardfork_id: String,
    /// L2 activation timestamp announced on L1.
    pub activation_timestamp: u64,
    /// Expected protocol version announced on L1.
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
}

/// Upgrade activation observed locally.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UpgradeActivation {
    /// Hardfork ID that activated.
    pub hardfork_id: String,
    /// Activation timestamp announced on L1.
    pub activation_timestamp: u64,
    /// Expected protocol version announced on L1.
    pub protocol_version: U256,
    /// Local L2 timestamp that reached or crossed the activation timestamp.
    pub l2_timestamp: u64,
}

/// Result of applying a signal update to local observer state.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpgradeSignalStateUpdate {
    /// The signal is identical to the previous signal.
    Unchanged,
    /// The signal changed and activation observation was reset.
    Changed,
}

/// Stateful activation tracker for one hardfork ID.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct UpgradeSignalState {
    /// Last signal read from L1.
    pub signal: Option<UpgradeSignal>,
    /// Whether activation has already been observed for the current signal.
    pub activation_observed: bool,
}

impl UpgradeSignalState {
    /// Creates an empty upgrade signal state tracker.
    pub const fn new() -> Self {
        Self { signal: None, activation_observed: false }
    }

    /// Applies a newly read signal.
    pub fn update_signal(&mut self, signal: UpgradeSignal) -> UpgradeSignalStateUpdate {
        let changed = match self.signal.as_ref() {
            Some(previous) => !previous.has_same_contract_values(&signal),
            None => true,
        };

        self.signal = Some(signal);
        if changed {
            self.activation_observed = false;
            UpgradeSignalStateUpdate::Changed
        } else {
            UpgradeSignalStateUpdate::Unchanged
        }
    }

    /// Observes an L2 timestamp and returns an activation event if this timestamp activates.
    pub fn observe_l2_timestamp(&mut self, l2_timestamp: u64) -> Option<UpgradeActivation> {
        if self.activation_observed {
            return None;
        }

        let signal = self.signal.as_ref()?;
        if signal.activation_timestamp == 0 {
            return None;
        }
        if l2_timestamp < signal.activation_timestamp {
            return None;
        }

        self.activation_observed = true;
        Some(UpgradeActivation {
            hardfork_id: signal.hardfork_id.clone(),
            activation_timestamp: signal.activation_timestamp,
            protocol_version: signal.protocol_version,
            l2_timestamp,
        })
    }
}

/// Records upgrade signal state transitions, logs, and metrics.
#[derive(Debug, Clone)]
pub struct UpgradeSignalMonitor {
    /// Observer configuration.
    pub config: UpgradeSignalConfig,
    /// Activation state by hardfork ID.
    pub states: BTreeMap<String, UpgradeSignalState>,
}

impl UpgradeSignalMonitor {
    /// Creates a monitor for the provided configuration.
    pub fn new(config: UpgradeSignalConfig) -> Self {
        UpgradeSignalMetrics::init();
        let mut states = BTreeMap::new();
        for hardfork_id in &config.hardfork_ids {
            UpgradeSignalMetrics::activation_observed(hardfork_id.clone()).set(0.0);
            states.insert(hardfork_id.clone(), UpgradeSignalState::new());
        }
        Self { config, states }
    }

    /// Records an L1 read error.
    pub fn record_l1_read_error(&self, error: &impl core::fmt::Display) {
        for hardfork_id in &self.config.hardfork_ids {
            UpgradeSignalMetrics::l1_read_errors_total(hardfork_id.clone()).increment(1);
        }
        warn!(
            target: "upgrade_signal",
            error = %error,
            hardfork_ids = ?self.config.hardfork_ids,
            contract_address = %self.config.contract_address,
            "failed to read L1 upgrade signal"
        );
    }

    /// Records an L2 timestamp read error.
    pub fn record_l2_timestamp_error(&self, error: &impl core::fmt::Display) {
        for hardfork_id in &self.config.hardfork_ids {
            UpgradeSignalMetrics::l2_timestamp_errors_total(hardfork_id.clone()).increment(1);
        }
        warn!(
            target: "upgrade_signal",
            error = %error,
            hardfork_ids = ?self.config.hardfork_ids,
            "failed to read L2 timestamp"
        );
    }

    /// Applies a signal read from L1 and records corresponding metrics.
    pub fn update_signal(&mut self, signal: UpgradeSignal) -> UpgradeSignalStateUpdate {
        let hardfork_id = signal.hardfork_id.clone();
        UpgradeSignalMetrics::activation_timestamp(hardfork_id.clone())
            .set(signal.activation_timestamp as f64);
        UpgradeSignalMetrics::expected_protocol_version(hardfork_id.clone())
            .set(Self::protocol_version_to_f64(signal.protocol_version));
        UpgradeSignalMetrics::last_l1_read_block(hardfork_id.clone())
            .set(signal.l1_block_number as f64);

        let update =
            self.states.entry(hardfork_id.clone()).or_default().update_signal(signal.clone());
        if matches!(update, UpgradeSignalStateUpdate::Changed) {
            UpgradeSignalMetrics::activation_observed(hardfork_id).set(0.0);
            info!(
                target: "upgrade_signal",
                hardfork_id = %signal.hardfork_id,
                l1_block_number = signal.l1_block_number,
                contract_address = %self.config.contract_address,
                protocol_version = %signal.protocol_version,
                activation_timestamp = signal.activation_timestamp,
                "updated L1 upgrade signal"
            );
        } else {
            debug!(
                target: "upgrade_signal",
                hardfork_id = %signal.hardfork_id,
                l1_block_number = signal.l1_block_number,
                protocol_version = %signal.protocol_version,
                activation_timestamp = signal.activation_timestamp,
                "L1 upgrade signal unchanged"
            );
        }

        update
    }

    /// Applies signals read from L1 and records corresponding metrics.
    pub fn update_schedule(&mut self, schedule: UpgradeSignalSchedule) {
        for signal in schedule.signals {
            self.update_signal(signal);
        }
    }

    /// Observes an L2 timestamp and records activation if the timestamp crosses the signal.
    pub fn observe_l2_timestamp(&mut self, l2_timestamp: u64) -> Vec<UpgradeActivation> {
        let mut activations = Vec::new();

        for state in self.states.values_mut() {
            let Some(activation) = state.observe_l2_timestamp(l2_timestamp) else {
                continue;
            };

            UpgradeSignalMetrics::activation_observed(activation.hardfork_id.clone()).set(1.0);
            UpgradeSignalMetrics::activation_observed_total(activation.hardfork_id.clone())
                .increment(1);

            info!(
                target: "upgrade_signal",
                hardfork_id = %activation.hardfork_id,
                l2_timestamp = activation.l2_timestamp,
                protocol_version = %activation.protocol_version,
                activation_timestamp = activation.activation_timestamp,
                "upgrade activation timestamp reached"
            );

            activations.push(activation);
        }

        activations
    }

    /// Converts a protocol version to a metric gauge value.
    pub fn protocol_version_to_f64(protocol_version: U256) -> f64 {
        protocol_version.to_string().parse::<f64>().unwrap_or(-1.0)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};

    use super::*;

    fn config() -> UpgradeSignalConfig {
        UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul")
    }

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
    fn state_observes_activation_once() {
        let mut state = UpgradeSignalState::new();

        assert_eq!(state.update_signal(signal(10)), UpgradeSignalStateUpdate::Changed);
        assert_eq!(state.observe_l2_timestamp(9), None);
        assert!(state.observe_l2_timestamp(10).is_some());
        assert_eq!(state.observe_l2_timestamp(11), None);
    }

    #[test]
    fn state_ignores_zero_activation_timestamp() {
        let mut state = UpgradeSignalState::new();

        state.update_signal(signal(0));

        assert_eq!(state.observe_l2_timestamp(0), None);
        assert_eq!(state.observe_l2_timestamp(1), None);
    }

    #[test]
    fn changed_signal_resets_activation() {
        let mut state = UpgradeSignalState::new();

        state.update_signal(signal(10));
        assert!(state.observe_l2_timestamp(10).is_some());
        assert_eq!(state.update_signal(signal(12)), UpgradeSignalStateUpdate::Changed);
        assert_eq!(state.observe_l2_timestamp(11), None);
        assert!(state.observe_l2_timestamp(12).is_some());
    }

    #[test]
    fn l1_block_update_does_not_reset_activation() {
        let mut state = UpgradeSignalState::new();
        let mut updated_signal = signal(10);

        state.update_signal(signal(10));
        assert!(state.observe_l2_timestamp(10).is_some());

        updated_signal.l1_block_number = 2;

        assert_eq!(state.update_signal(updated_signal), UpgradeSignalStateUpdate::Unchanged);
        assert_eq!(state.observe_l2_timestamp(11), None);
    }

    #[test]
    fn monitor_records_activation() {
        let mut monitor = UpgradeSignalMonitor::new(config());

        monitor.update_signal(signal(10));

        assert_eq!(monitor.observe_l2_timestamp(10).len(), 1);
        assert!(monitor.observe_l2_timestamp(12).is_empty());
    }
}
