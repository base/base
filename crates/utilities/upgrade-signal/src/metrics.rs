//! Metrics for upgrade signal schedule reads.

use alloy_primitives::U256;
use base_common_genesis::BaseUpgrade;

use crate::{UpgradeSignal, UpgradeSignalSchedule};

base_metrics::define_metrics! {
    base.upgrade_signal, struct = UpgradeSignalMetrics,
    #[describe("Configured activation timestamp read from L1")]
    #[label(hardfork)]
    activation_timestamp: gauge,
    #[describe("Minimum node protocol version read from L1")]
    #[label(hardfork)]
    expected_protocol_version: gauge,
    #[describe("Last L1 block number used for a successful upgrade signal read")]
    #[label(hardfork)]
    last_l1_read_block: gauge,
    #[describe("Total failed attempts to read the L1 upgrade signal")]
    #[label(hardfork)]
    l1_read_errors_total: counter,
    #[describe("Total observed L1 upgrade signal value changes while the node is live")]
    #[label(hardfork)]
    signal_updates_total: counter,
}

impl UpgradeSignalMetrics {
    /// Records all metrics derived from a successfully read schedule.
    pub fn record_schedule(schedule: &UpgradeSignalSchedule) {
        Self::init();
        for signal in &schedule.signals {
            Self::record_signal(signal);
        }
    }

    /// Records all metrics derived from a successfully read signal.
    pub fn record_signal(signal: &UpgradeSignal) {
        Self::init();
        let hardfork_id = signal.hardfork_id.contract_id().to_string();

        Self::activation_timestamp(hardfork_id.clone()).set(signal.activation_timestamp as f64);
        Self::expected_protocol_version(hardfork_id.clone())
            .set(Self::protocol_version_to_f64(signal.protocol_version));
        Self::last_l1_read_block(hardfork_id).set(signal.l1_block_number as f64);
    }

    /// Records a failed L1 read for one hardfork ID.
    pub fn record_l1_read_error(hardfork_id: BaseUpgrade) {
        Self::init();
        Self::l1_read_errors_total(hardfork_id.contract_id().to_string()).increment(1);
    }

    /// Records failed L1 reads for all configured hardfork IDs.
    pub fn record_l1_read_errors(hardfork_ids: &[BaseUpgrade]) {
        Self::init();
        for hardfork_id in hardfork_ids {
            Self::l1_read_errors_total(hardfork_id.contract_id().to_string()).increment(1);
        }
    }

    /// Records a live L1 signal value update for one hardfork ID.
    pub fn record_signal_update(hardfork_id: BaseUpgrade) {
        Self::init();
        Self::signal_updates_total(hardfork_id.contract_id().to_string()).increment(1);
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
