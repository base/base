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
    #[describe("Total failed attempts to apply a live upgrade signal schedule")]
    #[label(layer)]
    #[label(upgrade)]
    apply_failures_total: counter,
    #[describe("1 while the most recent live apply for an upgrade is failing, else 0")]
    #[label(layer)]
    #[label(upgrade)]
    apply_failed: gauge,
    #[describe("Total times the node failed closed on an unsupportable upgrade nearing activation")]
    #[label(layer)]
    #[label(upgrade)]
    fail_closed_total: counter,
}

impl UpgradeSignalMetrics {
    /// Records all metrics derived from a successfully read schedule.
    pub fn record_schedule(layer: UpgradeSignalMetricLayer, schedule: &UpgradeSignalSchedule) {
        Self::init();
        for signal in &schedule.signals {
            Self::record_signal(layer, schedule.l1_block_number, signal);
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
    pub fn record_signal(
        layer: UpgradeSignalMetricLayer,
        l1_block_number: u64,
        signal: &UpgradeSignal,
    ) {
        Self::init();
        let layer = layer.label();
        let upgrade_id = signal.upgrade_id.contract_id().to_string();

        Self::activation_timestamp(layer, upgrade_id.clone())
            .set(signal.activation_timestamp as f64);
        Self::expected_protocol_version(layer, upgrade_id.clone())
            .set(Self::protocol_version_to_f64(signal.protocol_version));
        Self::last_l1_read_block(layer, upgrade_id).set(l1_block_number as f64);
    }

    /// Records failed L1 reads for all contract-backed upgrades.
    pub fn record_l1_read_errors(layer: UpgradeSignalMetricLayer) {
        Self::init();
        for upgrade_id in BaseUpgrade::CONTRACT_VARIANTS {
            Self::l1_read_errors_total(layer.label(), upgrade_id.contract_id().to_string())
                .increment(1);
        }
    }

    /// Records failed L1 reads for all contract-backed upgrades across all enabled layers.
    pub fn record_l1_read_errors_for_layers(layers: &[UpgradeSignalMetricLayer]) {
        Self::init();
        for layer in layers {
            Self::record_l1_read_errors(*layer);
        }
    }

    /// Records a live L1 signal value update for one upgrade ID.
    pub fn record_signal_update(layer: UpgradeSignalMetricLayer, upgrade_id: BaseUpgrade) {
        Self::init();
        Self::signal_updates_total(layer.label(), upgrade_id.contract_id().to_string())
            .increment(1);
    }

    /// Records a failed live apply of a schedule, raising the sticky failure gauge per upgrade.
    pub fn record_apply_failure(layer: UpgradeSignalMetricLayer, schedule: &UpgradeSignalSchedule) {
        Self::init();
        for signal in &schedule.signals {
            let upgrade_id = signal.upgrade_id.contract_id().to_string();
            Self::apply_failures_total(layer.label(), upgrade_id.clone()).increment(1);
            Self::apply_failed(layer.label(), upgrade_id).set(1.0);
        }
    }

    /// Records a successful live apply of a schedule, clearing the sticky failure gauge per upgrade.
    pub fn record_apply_success(layer: UpgradeSignalMetricLayer, schedule: &UpgradeSignalSchedule) {
        Self::init();
        for signal in &schedule.signals {
            Self::apply_failed(layer.label(), signal.upgrade_id.contract_id().to_string()).set(0.0);
        }
    }

    /// Records that the node is failing closed because `signal`'s upgrade is unsupportable and its
    /// activation is within the halt lead time.
    ///
    /// Emitted immediately before the node halts, so the sticky `apply_failed` gauge stays raised.
    /// The counter is best-effort: it may not be scraped before the process exits, so the loud
    /// fatal log and the non-zero exit remain the primary signals.
    pub fn record_fail_closed(layer: UpgradeSignalMetricLayer, signal: &UpgradeSignal) {
        Self::init();
        Self::fail_closed_total(layer.label(), signal.upgrade_id.contract_id().to_string())
            .increment(1);
    }

    /// Converts a packed-semver protocol version to a compact metric gauge value.
    ///
    /// Decoded as `major * 1_000_000 + minor * 1_000 + patch` so the gauge stays readable
    /// (raw packed values exceed `f64` integer precision).
    ///
    /// Expects a contract-read packed semver value; non-semver inputs decode to garbage.
    pub fn protocol_version_to_f64(protocol_version: U256) -> f64 {
        let limbs = protocol_version.as_limbs();
        let major = limbs[1] >> 32;
        let minor = limbs[1] & u64::from(u32::MAX);
        let patch = limbs[0] >> 32;
        (major * 1_000_000 + minor * 1_000 + patch) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_packed_semver_protocol_version_to_metric_value() {
        let version = crate::UpgradeSignalDefaults::packed_protocol_version(1, 1, 0);
        assert_eq!(UpgradeSignalMetrics::protocol_version_to_f64(version), 1_001_000.0);
    }

    #[test]
    fn records_apply_outcome_without_panicking() {
        let schedule = UpgradeSignalSchedule::new(
            1,
            vec![UpgradeSignal {
                upgrade_id: BaseUpgrade::Azul,
                activation_timestamp: 42,
                protocol_version: U256::from(7),
            }],
        );

        UpgradeSignalMetrics::record_apply_failure(UpgradeSignalMetricLayer::Consensus, &schedule);
        UpgradeSignalMetrics::record_apply_success(UpgradeSignalMetricLayer::Consensus, &schedule);
    }
}
