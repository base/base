//! Metrics for upgrade signal schedule reads.

use alloy_primitives::U256;
use base_common_genesis::BaseUpgrade;

use crate::{UpgradeSignal, UpgradeSignalSchedule};

/// Upgrade signal metric layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeSignalMetricLayer {
    /// Execution layer metrics.
    Execution,
    /// Consensus layer metrics.
    Consensus,
}

impl UpgradeSignalMetricLayer {
    /// Returns the Prometheus label value for this metric layer.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Execution => "el",
            Self::Consensus => "cl",
        }
    }
}

base_metrics::define_metrics! {
    base.upgrade_signal, struct = UpgradeSignalMetrics,
    #[describe("Configured activation timestamp read from L1")]
    #[label(layer)]
    #[label(upgrade)]
    activation_timestamp: gauge,
    #[describe("Minimum node protocol version read from L1")]
    #[label(layer)]
    #[label(upgrade)]
    expected_protocol_version: gauge,
    #[describe("Last L1 block number used for a successful upgrade signal read")]
    #[label(layer)]
    #[label(upgrade)]
    last_l1_read_block: gauge,
    #[describe("Total failed attempts to read the L1 upgrade signal")]
    #[label(layer)]
    #[label(upgrade)]
    l1_read_errors_total: counter,
    #[describe("Total observed L1 upgrade signal value changes while the node is live")]
    #[label(layer)]
    #[label(upgrade)]
    signal_updates_total: counter,
}

impl UpgradeSignalMetrics {
    /// Records all metrics derived from a successfully read schedule.
    pub fn record_schedule(layer: UpgradeSignalMetricLayer, schedule: &UpgradeSignalSchedule) {
        Self::init();
        for signal in &schedule.signals {
            Self::record_signal(layer, signal);
        }
    }

    /// Records all metrics derived from a successfully read schedule for all enabled layers.
    pub fn record_schedule_for_layers(
        layers: &[UpgradeSignalMetricLayer],
        schedule: &UpgradeSignalSchedule,
    ) {
        Self::init();
        for layer in layers {
            Self::record_schedule(*layer, schedule);
        }
    }

    /// Records all metrics derived from a successfully read signal.
    pub fn record_signal(layer: UpgradeSignalMetricLayer, signal: &UpgradeSignal) {
        Self::init();
        let layer = layer.label();
        let upgrade_id = signal.upgrade_id.contract_id().to_string();

        Self::activation_timestamp(layer, upgrade_id.clone())
            .set(signal.activation_timestamp as f64);
        Self::expected_protocol_version(layer, upgrade_id.clone())
            .set(Self::protocol_version_to_f64(signal.protocol_version));
        Self::last_l1_read_block(layer, upgrade_id).set(signal.l1_block_number as f64);
    }

    /// Records a failed L1 read for one upgrade ID.
    pub fn record_l1_read_error(layer: UpgradeSignalMetricLayer, upgrade_id: BaseUpgrade) {
        Self::init();
        Self::l1_read_errors_total(layer.label(), upgrade_id.contract_id().to_string())
            .increment(1);
    }

    /// Records a failed L1 read for one upgrade ID across all enabled layers.
    pub fn record_l1_read_error_for_layers(
        layers: &[UpgradeSignalMetricLayer],
        upgrade_id: BaseUpgrade,
    ) {
        Self::init();
        for layer in layers {
            Self::record_l1_read_error(*layer, upgrade_id);
        }
    }

    /// Records failed L1 reads for all configured upgrade IDs.
    pub fn record_l1_read_errors(layer: UpgradeSignalMetricLayer, upgrade_ids: &[BaseUpgrade]) {
        Self::init();
        for upgrade_id in upgrade_ids {
            Self::l1_read_errors_total(layer.label(), upgrade_id.contract_id().to_string())
                .increment(1);
        }
    }

    /// Records failed L1 reads for all configured upgrade IDs across all enabled layers.
    pub fn record_l1_read_errors_for_layers(
        layers: &[UpgradeSignalMetricLayer],
        upgrade_ids: &[BaseUpgrade],
    ) {
        Self::init();
        for layer in layers {
            Self::record_l1_read_errors(*layer, upgrade_ids);
        }
    }

    /// Records a live L1 signal value update for one upgrade ID.
    pub fn record_signal_update(layer: UpgradeSignalMetricLayer, upgrade_id: BaseUpgrade) {
        Self::init();
        Self::signal_updates_total(layer.label(), upgrade_id.contract_id().to_string())
            .increment(1);
    }

    /// Converts a protocol version to a metric gauge value.
    pub fn protocol_version_to_f64(protocol_version: U256) -> f64 {
        protocol_version.to_string().parse::<f64>().unwrap_or(-1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_protocol_version_to_metric_value() {
        assert_eq!(UpgradeSignalMetrics::protocol_version_to_f64(U256::from(7)), 7.0);
    }
}
