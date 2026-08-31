//! Metrics shared by payload builders.

use std::time::Duration;

use crate::{
    FLOW_STANDARD, FLOW_VALIDITY, InclusionTracker, ParkedPredicateIndex, PredicateLoadTracker,
};

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

base_metrics::define_metrics! {
    base_builder,
    struct = BuilderMetrics,
    #[describe("Transactions included per block, segmented by flow")]
    #[label(flow)]
    txs_included_per_block: histogram,
    #[describe("Gas consumed by included transactions per block, segmented by flow")]
    #[label(flow)]
    tx_gas_used_per_block: histogram,
    #[describe("Per-block EIP-1559 priority-fee revenue in wei, segmented by flow")]
    #[label(flow)]
    priority_fee_revenue_wei: histogram,
    #[describe("Per-block EIP-1559 base-fee revenue in wei, segmented by flow")]
    #[label(flow)]
    base_fee_revenue_wei: histogram,
    #[describe("Per-block EIP-8130 coinbase-tip revenue in wei, segmented by flow")]
    #[label(flow)]
    coinbase_tip_revenue_wei: histogram,
    #[describe(
        "Per-included-transaction tip per gas (the builder priority score), tagged by flow cohort and bid mechanism"
    )]
    #[label(name = "flow", default = ["standard", "validity"])]
    #[label(name = "bid", default = ["coinbase_tip", "priority_fee"])]
    tip_per_gas: histogram,
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

impl BuilderMetrics {
    /// Records per-block inclusion and EIP-1559 fee revenue.
    ///
    /// Always emits one observation per built block, including zeros, so the
    /// histograms describe the full per-block distribution.
    pub fn record_inclusion(tracker: &InclusionTracker) {
        Self::txs_included_per_block(FLOW_STANDARD).record(tracker.standard.txs as f64);
        Self::txs_included_per_block(FLOW_VALIDITY).record(tracker.validity.txs as f64);
        Self::tx_gas_used_per_block(FLOW_STANDARD).record(tracker.standard.gas as f64);
        Self::tx_gas_used_per_block(FLOW_VALIDITY).record(tracker.validity.gas as f64);
        Self::priority_fee_revenue_wei(FLOW_STANDARD).record(tracker.standard.priority_fees_f64());
        Self::priority_fee_revenue_wei(FLOW_VALIDITY).record(tracker.validity.priority_fees_f64());
        Self::base_fee_revenue_wei(FLOW_STANDARD).record(tracker.standard.base_fees_f64());
        Self::base_fee_revenue_wei(FLOW_VALIDITY).record(tracker.validity.base_fees_f64());
        Self::coinbase_tip_revenue_wei(FLOW_STANDARD).record(tracker.standard.coinbase_tips_f64());
        Self::coinbase_tip_revenue_wei(FLOW_VALIDITY).record(tracker.validity.coinbase_tips_f64());
    }

    /// Records one included transaction's tip per gas by flow and bid mechanism.
    pub fn record_tip_per_gas(
        has_validity_predicates: bool,
        has_coinbase_tip: bool,
        tip_per_gas: f64,
    ) {
        let flow = if has_validity_predicates { FLOW_VALIDITY } else { FLOW_STANDARD };
        let bid = if has_coinbase_tip { "coinbase_tip" } else { "priority_fee" };
        Self::tip_per_gas(flow, bid).record(tip_per_gas);
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
