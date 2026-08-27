//! Metrics shared by payload builders that evaluate validity predicates.

use std::time::Duration;

use crate::{ParkedPredicateIndex, PredicateLoadTracker};

base_metrics::define_metrics! {
    base_builder,
    struct = ValidityMetrics,
    #[describe("Duration of rescanning parked transaction validity predicates in seconds")]
    validity_predicate_rescan_duration: histogram,
    #[describe(
        "Total validity predicate evaluation time per build, inclusive of state loads, in seconds"
    )]
    validity_predicate_eval_duration_per_block: histogram,
    #[describe(
        "Number of validity-predicate index buckets woken per build"
    )]
    predicate_bucket_wakeups: histogram,
    #[describe(
        "Depth of validity-predicate index buckets, sampled once per build"
    )]
    predicate_bucket_depth: histogram,
    #[describe(
        "Accounts read while evaluating validity predicates per block, counting every read"
    )]
    predicate_accounts_loaded_total: histogram,
    #[describe(
        "Distinct accounts read while evaluating validity predicates per block"
    )]
    predicate_accounts_loaded_unique: histogram,
    #[describe(
        "Storage slots read while evaluating validity predicates per block, counting every read including re-reads"
    )]
    predicate_slots_loaded_total: histogram,
    #[describe(
        "Distinct storage slots read while evaluating validity predicates per block"
    )]
    predicate_slots_loaded_unique: histogram,
    #[describe("Validity predicate evaluation attempts")]
    #[label(outcome)]
    validity_predicate_evaluations_total: counter,
}

impl ValidityMetrics {
    /// Records the total validity predicate evaluation time accumulated across one build.
    pub fn record_predicate_eval_duration(duration: Duration) {
        Self::validity_predicate_eval_duration_per_block().record(duration.as_secs_f64());
    }

    /// Records the block's accumulated validity-predicate state loads.
    pub fn record_predicate_loads(tracker: &PredicateLoadTracker) {
        if !tracker.has_activity() {
            return;
        }

        Self::predicate_accounts_loaded_total().record(tracker.account_reads() as f64);
        Self::predicate_accounts_loaded_unique().record(tracker.unique_accounts() as f64);
        Self::predicate_slots_loaded_total().record(tracker.slot_reads() as f64);
        Self::predicate_slots_loaded_unique().record(tracker.unique_slots() as f64);
    }

    /// Records validity-predicate index bucket wakeups and depth distribution for one build.
    pub fn record_predicate_index_diagnostics<T>(wakeups: u64, index: &ParkedPredicateIndex<T>) {
        Self::predicate_bucket_wakeups().record(wakeups as f64);
        for depth in index.bucket_depths() {
            Self::predicate_bucket_depth().record(depth as f64);
        }
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{Address, B256, U256};
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;
    use crate::ValidityPredicateKey;

    #[test]
    fn records_predicate_metrics() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let account = Address::with_last_byte(1);
        let slot = U256::from(7);
        let mut tracker = PredicateLoadTracker::default();
        tracker.record_account(account);
        tracker.record_account(account);
        tracker.record_slot(account, slot);

        let mut index = ParkedPredicateIndex::default();
        index.park(B256::with_last_byte(1), (), ValidityPredicateKey::Balance(account));

        metrics::with_local_recorder(&recorder, || {
            ValidityMetrics::record_predicate_eval_duration(Duration::from_millis(500));
            ValidityMetrics::record_predicate_loads(&tracker);
            ValidityMetrics::record_predicate_index_diagnostics(3, &index);
        });

        let rendered = handle.render();
        assert!(
            rendered.contains("base_builder_validity_predicate_eval_duration_per_block_sum 0.5")
        );
        assert!(rendered.contains("base_builder_predicate_accounts_loaded_total_sum 2"));
        assert!(rendered.contains("base_builder_predicate_accounts_loaded_unique_sum 1"));
        assert!(rendered.contains("base_builder_predicate_slots_loaded_total_sum 1"));
        assert!(rendered.contains("base_builder_predicate_slots_loaded_unique_sum 1"));
        assert!(rendered.contains("base_builder_predicate_bucket_wakeups_sum 3"));
        assert!(rendered.contains("base_builder_predicate_bucket_depth_sum 1"));
    }

    #[test]
    fn predicate_load_metrics_ignore_builds_without_activity() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            ValidityMetrics::record_predicate_loads(&PredicateLoadTracker::default());
        });

        assert!(!handle.render().contains("predicate_accounts_loaded_total"));
    }
}
