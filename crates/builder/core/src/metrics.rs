//! Builder metrics collected during block and flashblock construction.

use std::time::Duration;

use crate::{
    ExecutionInfo, FlashblockDiagnostics, ParkedPredicateIndex, PredicateLoadTracker,
    ResourceLimits,
};

const PRIORITY_FEE_THRESHOLDS_WEI: [(&str, u64); 3] =
    [("100wei", 100), ("100kwei", 100_000), ("1mwei", 1_000_000)];

base_metrics::define_metrics! {
    base_builder,
    struct = BuilderMetrics,
    #[describe("Block built success")]
    block_built_success: counter,
    #[describe("Block synced success")]
    block_synced_success: counter,
    #[describe("Number of flashblocks added to block (Total per block)")]
    flashblock_count: histogram,
    #[describe("Histogram of the time taken to build a block")]
    total_block_built_duration: histogram,
    #[describe("Latest time taken to build a block")]
    total_block_built_gauge: gauge,
    #[describe("Histogram of the time taken to build a Flashblock")]
    flashblock_build_duration: histogram,
    #[describe("Histogram of the time taken to sync a Flashblock")]
    flashblock_sync_duration: histogram,
    #[describe("Flashblock UTF8 payload byte size histogram")]
    flashblock_byte_size_histogram: histogram,
    #[describe("Histogram of transactions in a Flashblock")]
    flashblock_num_tx_histogram: histogram,
    #[describe("Number of invalid blocks")]
    invalid_built_blocks_count: counter,
    #[describe("Number of invalid synced blocks")]
    invalid_synced_blocks_count: counter,
    #[describe("Histogram of fetching transactions from the pool duration")]
    transaction_pool_fetch_duration: histogram,
    #[describe("Latest time taken to fetch tx from the pool")]
    transaction_pool_fetch_gauge: gauge,
    #[describe("Histogram of state root calculation duration")]
    state_root_calculation_duration: histogram,
    #[describe("Latest state root calculation duration")]
    state_root_calculation_gauge: gauge,
    #[describe("Histogram of sequencer transaction execution duration")]
    sequencer_tx_duration: histogram,
    #[describe("Latest sequencer transaction execution duration")]
    sequencer_tx_gauge: gauge,
    #[describe("Histogram of state merge transitions duration")]
    state_transition_merge_duration: histogram,
    #[describe("Latest state merge transitions duration")]
    state_transition_merge_gauge: gauge,
    #[describe("Histogram of the duration of payload simulation of all transactions")]
    payload_transaction_simulation_duration: histogram,
    #[describe("Latest payload simulation of all transactions duration")]
    payload_transaction_simulation_gauge: gauge,
    #[describe("Number of transaction considered for inclusion in the block")]
    payload_num_tx_considered: histogram,
    #[describe("Latest number of transactions considered for inclusion in the block")]
    payload_num_tx_considered_gauge: gauge,
    #[describe("Payload byte size histogram")]
    payload_byte_size: histogram,
    #[describe("Latest Payload byte size")]
    payload_byte_size_gauge: gauge,
    #[describe("Histogram of transactions in the payload")]
    payload_num_tx: histogram,
    #[describe("Latest number of transactions in the payload")]
    payload_num_tx_gauge: gauge,
    #[describe("Histogram of transactions in the payload that were successfully simulated")]
    payload_num_tx_simulated: histogram,
    #[describe("Latest number of transactions in the payload that were successfully simulated")]
    payload_num_tx_simulated_gauge: gauge,
    #[describe("Histogram of transactions in the payload that were successfully simulated")]
    payload_num_tx_simulated_success: histogram,
    #[describe("Latest number of transactions in the payload that were successfully simulated")]
    payload_num_tx_simulated_success_gauge: gauge,
    #[describe("Histogram of transactions in the payload that failed simulation")]
    payload_num_tx_simulated_fail: histogram,
    #[describe("Latest number of transactions in the payload that failed simulation")]
    payload_num_tx_simulated_fail_gauge: gauge,
    #[describe("Histogram of gas used by successful transactions")]
    successful_tx_gas_used: histogram,
    #[describe("Histogram of gas used by reverted transactions")]
    reverted_tx_gas_used: histogram,
    #[describe("Gas used by reverted transactions in the latest block")]
    payload_reverted_tx_gas_used: gauge,
    #[describe(
        "Histogram of local builder EVM transaction execution/simulation duration in seconds"
    )]
    tx_simulation_duration: histogram,
    #[describe("Byte size of transactions")]
    tx_byte_size: histogram,
    #[describe("How much less flashblocks we issue to be on time with block construction")]
    reduced_flashblocks_number: histogram,
    #[describe("How much less flashblocks we issued in reality, comparing to calculated number for block")]
    missing_flashblocks_count: histogram,
    #[describe("How much time we have deducted from block building time")]
    flashblocks_time_drift: histogram,
    #[describe("Time offset we used for first flashblock")]
    first_flashblock_time_offset: histogram,
    #[describe("Count of the number of times transactions had metering information")]
    metering_known_transaction: counter,
    #[describe("Count of the number of times transactions did not have any metering information")]
    metering_unknown_transaction: counter,
    #[describe("Number of LRU evictions from MeteringStore")]
    metering_store_lru_evictions: counter,
    #[describe("Number of entries in the metering store that expired due to TTL")]
    metering_store_ttl_expirations: counter,
    #[describe("Size of MeteringStore")]
    metering_store_size: gauge,
    #[describe("Transactions inserted into the rejection cache")]
    rejection_cache_insertions: counter,
    #[describe("Transactions skipped by the rejection cache (P2P re-insertion prevented)")]
    rejection_cache_hits: counter,
    #[describe("Number of entries in the rejection cache")]
    rejection_cache_size: gauge,
    #[describe("Duration of rescanning parked transaction validity predicates in seconds")]
    validity_predicate_rescan_duration: histogram,
    #[describe(
        "Total validity predicate evaluation time per flashblock build, inclusive of state loads, in seconds"
    )]
    validity_predicate_eval_duration_per_block: histogram,
    #[describe(
        "Number of validity-predicate index buckets woken (watched balance or storage slot changed), per flashblock build"
    )]
    predicate_bucket_wakeups: histogram,
    #[describe(
        "Depth (parked transaction count) of validity-predicate index buckets, sampled once per flashblock build"
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
        "Distinct storage slots read while evaluating validity predicates per block (predicate state footprint)"
    )]
    predicate_slots_loaded_unique: histogram,
    #[describe("Validity predicate evaluation attempts")]
    #[label(outcome)]
    validity_predicate_evaluations_total: counter,
    #[describe("Shadow validity injection decisions")]
    #[label(outcome)]
    shadow_validity_injection_total: counter,
    #[describe("Transactions skipped because metering data has not yet arrived")]
    metering_data_pending_skip: counter,
    #[describe("Transactions rejected by per-tx DA size limit")]
    tx_da_size_exceeded_total: counter,
    #[describe("Transactions rejected by block DA size limit")]
    block_da_size_exceeded_total: counter,
    #[describe("Transactions rejected by DA footprint limit (post-Jovian, protocol-enforced)")]
    da_footprint_exceeded_total: counter,
    #[describe("Transactions rejected by gas limit (protocol-enforced)")]
    gas_limit_exceeded_total: counter,
    #[describe("Transactions rejected by block uncompressed size limit")]
    block_uncompressed_size_exceeded_total: counter,
    #[describe("Cumulative uncompressed block size at end of block")]
    block_uncompressed_size: histogram,
    #[describe("Transactions that would be rejected by execution metering limits")]
    resource_limit_would_reject_total: counter,
    #[describe("Transactions that exceeded per-tx execution time limit")]
    tx_execution_time_exceeded_total: counter,
    #[describe("Histogram of (predicted - actual) execution time per transaction in microseconds")]
    execution_time_prediction_error_us: histogram,
    #[describe("Distribution of predicted execution times from metering service (microseconds)")]
    tx_predicted_execution_time_us: histogram,
    #[describe("Flashblock selection total")]
    #[label(flashblock_index)]
    #[label(outcome)]
    flashblock_selection_total: counter,
    #[describe("Flashblock min priority fee above threshold total")]
    #[label(flashblock_index)]
    #[label(threshold)]
    flashblock_min_priority_fee_above_threshold_total: counter,
    #[describe("Flashblock txs considered")]
    #[label(flashblock_index)]
    flashblock_txs_considered: histogram,
    #[describe("Flashblock rejections total")]
    #[label(flashblock_index)]
    #[label(reason)]
    flashblock_rejections_total: counter,
    #[describe("Flashblock txs included")]
    #[label(flashblock_index)]
    flashblock_txs_included: histogram,
    #[describe("Flashblock txs deferred by parking")]
    #[label(flashblock_index)]
    flashblock_txs_deferred: histogram,
    #[describe("Flashblock txs rejected")]
    #[label(flashblock_index)]
    flashblock_txs_rejected: histogram,
    #[describe("Flashblock min priority fee in wei")]
    #[label(flashblock_index)]
    flashblock_min_priority_fee_wei: histogram,
    #[describe("Flashblock gas headroom")]
    #[label(flashblock_index)]
    flashblock_gas_headroom: histogram,
    #[describe("Flashblock gas headroom percentage")]
    #[label(flashblock_index)]
    flashblock_gas_headroom_pct: histogram,
    #[describe("Flashblock DA bytes used")]
    #[label(flashblock_index)]
    flashblock_da_bytes_used: histogram,
    #[describe("Flashblock DA headroom in bytes")]
    #[label(flashblock_index)]
    flashblock_da_headroom_bytes: histogram,
    #[describe("Priority fee of rejected transactions")]
    #[label(reason)]
    rejected_tx_priority_fee: histogram,
    #[describe("Actual execution time for transactions without metering data (microseconds)")]
    unmetered_tx_actual_execution_time_us: histogram,
    #[describe("Metering responses that arrived after unmetered payload inclusion")]
    metering_late_arrival_total: counter,
    #[describe("Time between unmetered payload inclusion and metering data arrival (milliseconds)")]
    metering_late_arrival_latency_ms: histogram,
    #[describe("Execution time from late-arriving metering data (microseconds)")]
    metering_late_arrival_execution_time_us: histogram,
    #[describe("Number of accounts modified by a transaction (from EVM post-state)")]
    tx_accounts_modified: histogram,
    #[describe("Number of storage slots modified by a transaction (from EVM post-state)")]
    tx_storage_slots_modified: histogram,
    #[describe("Rejected transaction batch drops due to full forwarding channel")]
    rejected_tx_channel_drops: counter,
    #[describe("Rejected transaction drops due to per-block accumulation limit")]
    rejected_tx_per_block_drops: counter,
    #[describe("Rejected txs forwarded to audit-archiver")]
    rejected_txs_forwarded: counter,
    #[describe("Number of failed rejected transaction batch forwards to audit-archiver")]
    rejected_tx_forward_failures: counter,
    #[describe("Builder transaction events successfully enqueued")]
    #[label(event_type)]
    builder_transaction_events_emitted: counter,
    #[describe("Builder transaction events dropped before enqueue")]
    #[label(event_type)]
    #[label(reason)]
    builder_transaction_events_dropped: counter,
    #[describe(
        "Per-included-transaction tip per gas (the builder priority score), tagged by flow cohort and bid mechanism"
    )]
    #[label(name = "flow", default = ["standard", "validity"])]
    #[label(name = "bid", default = ["coinbase_tip", "priority_fee"])]
    tip_per_gas: histogram,
}

impl BuilderMetrics {
    /// Records the total validity predicate evaluation time accumulated across a
    /// single build iteration, inclusive of the state loads each evaluation performs.
    pub fn record_predicate_eval_duration(duration: Duration) {
        Self::validity_predicate_eval_duration_per_block().record(duration.as_secs_f64());
    }

    /// Records per-flashblock selection diagnostics as labeled metrics.
    pub fn record_flashblock_diagnostics(
        flashblock_index: u64,
        diag: &FlashblockDiagnostics,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
    ) {
        let flashblock_index = flashblock_index.to_string();
        Self::flashblock_selection_total(
            flashblock_index.clone(),
            diag.selection_outcome().as_str(),
        )
        .increment(1);

        Self::flashblock_txs_considered(flashblock_index.clone())
            .record(diag.txs_considered as f64);
        Self::flashblock_txs_included(flashblock_index.clone()).record(diag.txs_included as f64);
        Self::flashblock_txs_deferred(flashblock_index.clone()).record(diag.txs_deferred as f64);
        Self::flashblock_txs_rejected(flashblock_index.clone())
            .record(diag.txs_rejected_total() as f64);

        if let Some(min_priority_fee) = diag.min_priority_fee {
            Self::flashblock_min_priority_fee_wei(flashblock_index.clone())
                .record(min_priority_fee as f64);
            for (threshold, threshold_wei) in PRIORITY_FEE_THRESHOLDS_WEI {
                if min_priority_fee > threshold_wei {
                    Self::flashblock_min_priority_fee_above_threshold_total(
                        flashblock_index.clone(),
                        threshold,
                    )
                    .increment(1);
                }
            }
        }

        let gas_headroom = limits.block_gas_limit.saturating_sub(info.cumulative_gas_used);
        Self::flashblock_gas_headroom(flashblock_index.clone()).record(gas_headroom as f64);
        if limits.block_gas_limit > 0 {
            Self::flashblock_gas_headroom_pct(flashblock_index.clone())
                .record(gas_headroom as f64 / limits.block_gas_limit as f64 * 100.0);
        }

        Self::flashblock_da_bytes_used(flashblock_index.clone())
            .record(info.cumulative_da_bytes_used as f64);
        if let Some(block_data_limit) = limits.block_data_limit {
            Self::flashblock_da_headroom_bytes(flashblock_index.clone())
                .record(block_data_limit.saturating_sub(info.cumulative_da_bytes_used) as f64);
        }

        for (reason, count) in diag.rejection_counts() {
            if count == 0 {
                continue;
            }
            Self::flashblock_rejections_total(flashblock_index.clone(), reason).increment(count);
        }
    }

    /// Records the block's accumulated validity-predicate state loads as
    /// per-block histogram observations (total and distinct accounts/slots).
    ///
    /// Emits nothing when the block carried no validity transactions, so the
    /// histograms are not diluted with zero observations from ordinary blocks.
    pub fn record_predicate_loads(tracker: &PredicateLoadTracker) {
        if !tracker.has_activity() {
            return;
        }

        Self::predicate_accounts_loaded_total().record(tracker.account_reads() as f64);
        Self::predicate_accounts_loaded_unique().record(tracker.unique_accounts() as f64);
        Self::predicate_slots_loaded_total().record(tracker.slot_reads() as f64);
        Self::predicate_slots_loaded_unique().record(tracker.unique_slots() as f64);
    }

    /// Records payload builder metrics.
    pub fn set_payload_builder_metrics(
        payload_transaction_simulation_time: f64,
        num_txs_considered: f64,
        num_txs_simulated: f64,
        num_txs_simulated_success: f64,
        num_txs_simulated_fail: f64,
        reverted_gas_used: f64,
    ) {
        Self::payload_transaction_simulation_duration().record(payload_transaction_simulation_time);
        Self::payload_transaction_simulation_gauge().set(payload_transaction_simulation_time);
        Self::payload_num_tx_considered().record(num_txs_considered);
        Self::payload_num_tx_considered_gauge().set(num_txs_considered);
        Self::payload_num_tx_simulated().record(num_txs_simulated);
        Self::payload_num_tx_simulated_gauge().set(num_txs_simulated);
        Self::payload_num_tx_simulated_success().record(num_txs_simulated_success);
        Self::payload_num_tx_simulated_success_gauge().set(num_txs_simulated_success);
        Self::payload_num_tx_simulated_fail().record(num_txs_simulated_fail);
        Self::payload_num_tx_simulated_fail_gauge().set(num_txs_simulated_fail);
        Self::payload_reverted_tx_gas_used().set(reverted_gas_used);
    }

    /// Records validity-predicate index bucket wakeups and depth distribution for one flashblock build.
    pub fn record_predicate_index_diagnostics<T>(wakeups: u64, index: &ParkedPredicateIndex<T>) {
        Self::predicate_bucket_wakeups().record(wakeups as f64);
        for depth in index.bucket_depths() {
            Self::predicate_bucket_depth().record(depth as f64);
        }
    }

    /// Records one included transaction's tip per gas.
    ///
    /// The value is the builder's existing inclusion priority score
    /// (`effective_tip_per_gas` / tip-per-gas-limit) — no execution result or
    /// price feed is required. Observations are tagged `flow=validity` only when
    /// the transaction carries validity predicates, otherwise `flow=standard`.
    /// Bid mechanism is independent of flow: `bid=coinbase_tip` only when
    /// `TxEip8130::coinbase_tip` returns `Some` (a statically-analyzable
    /// phase-0 coinbase tip). EIP-8130 without that, and every non-8130
    /// transaction, uses `bid=priority_fee`.
    pub fn record_tip_per_gas(
        has_validity_predicates: bool,
        has_statically_analyzable_tip: bool,
        tip_per_gas: f64,
    ) {
        let flow = if has_validity_predicates { "validity" } else { "standard" };
        let bid = if has_statically_analyzable_tip { "coinbase_tip" } else { "priority_fee" };
        Self::tip_per_gas(flow, bid).record(tip_per_gas);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;
    use crate::ValidityPredicateKey;

    #[test]
    fn record_flashblock_diagnostics_emits_labeled_metrics() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let diag = FlashblockDiagnostics {
            txs_considered: 6,
            txs_included: 3,
            txs_deferred: 2,
            txs_rejected_gas: 2,
            txs_rejected_da: 1,
            txs_rejected_metering_data_pending: 1,
            min_priority_fee: Some(200_000),
            ..Default::default()
        };
        let info = ExecutionInfo {
            cumulative_gas_used: 60,
            cumulative_da_bytes_used: 15,
            ..Default::default()
        };
        let limits = ResourceLimits {
            block_gas_limit: 100,
            block_data_limit: Some(20),
            ..Default::default()
        };

        metrics::with_local_recorder(&recorder, || {
            BuilderMetrics::record_flashblock_diagnostics(7, &diag, &info, &limits);
        });

        let rendered = handle.render();
        assert!(rendered.contains(
            "base_builder_flashblock_selection_total{flashblock_index=\"7\",outcome=\"pool_drained\"} 1"
        ));
        assert!(rendered.contains(
            "base_builder_flashblock_rejections_total{flashblock_index=\"7\",reason=\"gas_limit\"} 2"
        ));
        assert!(rendered.contains(
            "base_builder_flashblock_rejections_total{flashblock_index=\"7\",reason=\"da_size\"} 1"
        ));
        assert!(rendered.contains(
            "base_builder_flashblock_rejections_total{flashblock_index=\"7\",reason=\"metering_data_pending\"} 1"
        ));
        assert!(
            rendered.contains("base_builder_flashblock_txs_included_sum{flashblock_index=\"7\"} 3")
        );
        assert!(
            rendered
                .contains("base_builder_flashblock_txs_considered_sum{flashblock_index=\"7\"} 6")
        );
        assert!(
            rendered.contains("base_builder_flashblock_txs_deferred_sum{flashblock_index=\"7\"} 2")
        );
        assert!(
            rendered
                .contains("base_builder_flashblock_gas_headroom_sum{flashblock_index=\"7\"} 40")
        );
        assert!(rendered.contains(
            "base_builder_flashblock_min_priority_fee_above_threshold_total{flashblock_index=\"7\",threshold=\"100wei\"} 1"
        ));
    }

    #[test]
    fn record_predicate_eval_duration_emits_histogram_in_seconds() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            // 500ms accumulated across a build iteration -> 0.5 seconds.
            BuilderMetrics::record_predicate_eval_duration(Duration::from_millis(500));
        });

        let rendered = handle.render();
        assert!(
            rendered.contains("base_builder_validity_predicate_eval_duration_per_block_count 1"),
            "expected a single observation, got: {rendered}"
        );
        assert!(
            rendered.contains("base_builder_validity_predicate_eval_duration_per_block_sum 0.5"),
            "expected 0.5s recorded, got: {rendered}"
        );
    }

    #[test]
    fn record_predicate_index_diagnostics_emits_wakeups_and_bucket_depths() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut index = ParkedPredicateIndex::default();
        index.park(
            B256::with_last_byte(1),
            (),
            ValidityPredicateKey::Balance(Address::with_last_byte(1)),
        );
        index.park(
            B256::with_last_byte(2),
            (),
            ValidityPredicateKey::Balance(Address::with_last_byte(1)),
        );
        index.park(
            B256::with_last_byte(3),
            (),
            ValidityPredicateKey::Balance(Address::with_last_byte(2)),
        );

        metrics::with_local_recorder(&recorder, || {
            BuilderMetrics::record_predicate_index_diagnostics(3, &index);
        });

        let rendered = handle.render();
        assert!(rendered.contains("base_builder_predicate_bucket_wakeups_sum 3"));
        assert!(rendered.contains("base_builder_predicate_bucket_depth_count 2"));
        assert!(rendered.contains("base_builder_predicate_bucket_depth_sum 3"));
    }

    #[test]
    fn record_predicate_loads_emits_total_and_unique_histograms() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let account = Address::with_last_byte(1);
        let slot = U256::from(7);
        let mut tracker = PredicateLoadTracker::default();
        // Account read twice; slot read three times — one distinct location each.
        tracker.record_account(account);
        tracker.record_account(account);
        tracker.record_slot(account, slot);
        tracker.record_slot(account, slot);
        tracker.record_slot(account, slot);

        metrics::with_local_recorder(&recorder, || {
            BuilderMetrics::record_predicate_loads(&tracker);
        });

        let rendered = handle.render();
        assert!(rendered.contains("base_builder_predicate_accounts_loaded_total_sum 2"));
        assert!(rendered.contains("base_builder_predicate_accounts_loaded_unique_sum 1"));
        assert!(rendered.contains("base_builder_predicate_slots_loaded_total_sum 3"));
        assert!(rendered.contains("base_builder_predicate_slots_loaded_unique_sum 1"));
    }

    #[test]
    fn record_predicate_loads_emits_nothing_without_activity() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            BuilderMetrics::record_predicate_loads(&PredicateLoadTracker::default());
        });

        assert!(!handle.render().contains("predicate_accounts_loaded_total"));
    }

    #[test]
    fn record_tip_per_gas_tags_flow_and_bid() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            // Standard EIP-1559: flow=standard, bid=priority_fee.
            BuilderMetrics::record_tip_per_gas(false, false, 10.0);
            BuilderMetrics::record_tip_per_gas(false, false, 30.0);
            // Pre-8130 validity: flow=validity, bid=priority_fee.
            BuilderMetrics::record_tip_per_gas(true, false, 50.0);
            // EIP-8130 with predicates and a static phase-0 tip.
            BuilderMetrics::record_tip_per_gas(true, true, 80.0);
            // EIP-8130 without predicates, but with a static phase-0 tip.
            BuilderMetrics::record_tip_per_gas(false, true, 20.0);
            // EIP-8130 without a statically-analyzable tip: bid=priority_fee.
            BuilderMetrics::record_tip_per_gas(false, false, 5.0);
            BuilderMetrics::record_tip_per_gas(true, false, 15.0);
        });

        let rendered = handle.render();
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_count{bid=\"priority_fee\",flow=\"standard\"} 3"
            ),
            "expected three standard priority-fee observations, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_sum{bid=\"priority_fee\",flow=\"standard\"} 45"
            ),
            "expected standard priority-fee sum 45, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_count{bid=\"priority_fee\",flow=\"validity\"} 2"
            ),
            "expected two validity priority-fee observations, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_sum{bid=\"priority_fee\",flow=\"validity\"} 65"
            ),
            "expected validity priority-fee sum 65, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_count{bid=\"coinbase_tip\",flow=\"validity\"} 1"
            ),
            "expected one 8130 validity observation, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_sum{bid=\"coinbase_tip\",flow=\"validity\"} 80"
            ),
            "expected 8130 validity sum 80, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_count{bid=\"coinbase_tip\",flow=\"standard\"} 1"
            ),
            "expected one 8130 standard observation, got: {rendered}"
        );
        assert!(
            rendered.contains(
                "base_builder_tip_per_gas_sum{bid=\"coinbase_tip\",flow=\"standard\"} 20"
            ),
            "expected 8130 standard sum 20, got: {rendered}"
        );
    }
}
