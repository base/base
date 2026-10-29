//! Metrics shared by payload builders.

use std::time::Duration;

use crate::{
    FLOW_STANDARD, FLOW_VALIDITY, InclusionTracker, ParkedPredicateIndex, PredicateLoadTracker,
    RestingCarryOver,
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
        "Supported validity transaction candidates evaluated during the primary scan per build"
    )]
    validity_predicate_candidates_evaluated_per_build: histogram,
    #[describe(
        "Validity transaction candidates deferred without evaluation after the predicate budget was exhausted per build"
    )]
    validity_predicate_candidates_deferred_per_build: histogram,
    #[describe(
        "Fraction of supported primary-scan validity transaction candidates evaluated per build"
    )]
    validity_predicate_evaluation_coverage_per_build: histogram,
    #[describe(
        "Builds that deferred at least one primary or rescan validity transaction after exhausting the predicate evaluation budget"
    )]
    validity_predicate_eval_cutoff_builds_total: counter,
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
    #[describe("Resting validity transactions carried into a block build from the previous block")]
    resting_predicate_carried: histogram,
    #[describe("Resting validity transactions released from the carried set, by reason")]
    #[label(reason)]
    resting_predicate_carry_released_total: counter,
    #[describe("Block builds that discarded carried resting validity transactions because the parent was not the last block the builder finished")]
    resting_predicate_carry_resets_total: counter,
    #[describe("Resting validity transactions parked in the transaction iterator without reaching the build loop")]
    resting_predicate_parked_total: counter,
    #[describe("Resting validity transactions whose predicates matched when evaluated in shadow mode")]
    resting_predicate_shadow_mismatches_total: counter,
    #[describe("Of the parked resting validity transactions, those carried over from an earlier block")]
    resting_predicate_carried_parked_total: counter,
    #[describe("Duration of carrying resting validity transactions into a block build, on the path before flashblock scheduling, in seconds")]
    resting_predicate_begin_block_duration: histogram,
    #[describe("Duration of the periodic sweep of resting validity transactions no build yielded, run off the build path, in seconds")]
    resting_predicate_sweep_duration: histogram,
    #[describe("Time a block build waited for the previous block's resting sweep to finish, in seconds")]
    resting_predicate_sweep_wait_duration: histogram,
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

    /// Records primary-scan predicate evaluation coverage and whether the build exhausted its
    /// evaluation budget.
    ///
    /// Counts are emitted for every completed build, including builds with no validity candidates.
    /// Coverage is emitted only when at least one supported candidate was evaluated or deferred.
    pub fn record_predicate_evaluation_coverage(evaluated: u64, deferred: u64, cutoff_hit: bool) {
        Self::validity_predicate_candidates_evaluated_per_build().record(evaluated as f64);
        Self::validity_predicate_candidates_deferred_per_build().record(deferred as f64);

        let candidates = evaluated + deferred;
        if candidates > 0 {
            Self::validity_predicate_evaluation_coverage_per_build()
                .record(evaluated as f64 / candidates as f64);
        }
        if cutoff_hit {
            Self::validity_predicate_eval_cutoff_builds_total().increment(1);
        }
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

    /// Records what carrying resting validity transactions into a new block did, and how long it
    /// took.
    pub fn record_resting_carry_over(carry_over: &RestingCarryOver, duration: Duration) {
        Self::resting_predicate_begin_block_duration().record(duration.as_secs_f64());
        if carry_over.reset {
            Self::resting_predicate_carry_resets_total().increment(1);
        }
        for (reason, released) in [
            ("pre_block_state", carry_over.released_by_state),
            ("block_number", carry_over.released_by_block_number),
        ] {
            if released > 0 {
                Self::resting_predicate_carry_released_total(reason).increment(released as u64);
            }
        }
        Self::resting_predicate_carried().record(carry_over.carried as f64);
    }

    /// Records one sweep of resting validity transactions no build yielded.
    pub fn record_resting_unseen_sweep(released: usize, duration: Duration) {
        if released > 0 {
            Self::resting_predicate_carry_released_total("unseen").increment(released as u64);
        }
        Self::resting_predicate_sweep_duration().record(duration.as_secs_f64());
    }

    /// Records how long a block build waited for the previous block's resting sweep.
    pub fn record_resting_sweep_wait(waited: Duration) {
        Self::resting_predicate_sweep_wait_duration().record(waited.as_secs_f64());
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
    use base_execution_txpool::{ValidityOperator, ValidityPredicate};
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

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
        index.park(
            B256::with_last_byte(1),
            (),
            ValidityPredicate::Balance {
                address: account,
                op: ValidityOperator::Equal,
                value: U256::ZERO,
            },
        );

        metrics::with_local_recorder(&recorder, || {
            ValidityMetrics::record_predicate_eval_duration(Duration::from_millis(500));
            ValidityMetrics::record_predicate_evaluation_coverage(4, 1, true);
            ValidityMetrics::record_predicate_loads(&tracker);
            ValidityMetrics::record_predicate_index_diagnostics(3, &index);
        });

        let rendered = handle.render();
        assert!(
            rendered.contains("base_builder_validity_predicate_eval_duration_per_block_sum 0.5")
        );
        assert!(
            rendered
                .contains("base_builder_validity_predicate_candidates_evaluated_per_build_sum 4")
        );
        assert!(
            rendered
                .contains("base_builder_validity_predicate_candidates_deferred_per_build_sum 1")
        );
        assert!(
            rendered
                .contains("base_builder_validity_predicate_evaluation_coverage_per_build_sum 0.8")
        );
        assert!(rendered.contains("base_builder_validity_predicate_eval_cutoff_builds_total 1"));
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
