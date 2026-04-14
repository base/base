//! Builder metrics collected during block and flashblock construction.

use crate::{ExecutionInfo, FlashblockDiagnostics, ResourceLimits};

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
    #[describe("Histogram of tx simulation duration")]
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
    #[describe("Size of MeteringStore")]
    metering_store_size: gauge,
    #[describe("Transactions inserted into the rejection cache")]
    rejection_cache_insertions: counter,
    #[describe("Transactions skipped by the rejection cache (P2P re-insertion prevented)")]
    rejection_cache_hits: counter,
    #[describe("Number of entries in the rejection cache")]
    rejection_cache_size: gauge,
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
    #[describe("Transactions that exceeded flashblock execution time budget")]
    flashblock_execution_time_exceeded_total: counter,
    #[describe("Transactions that exceeded block state root gas limit")]
    block_state_root_gas_exceeded_total: counter,
    #[describe("Histogram of (predicted - actual) execution time per transaction in microseconds")]
    execution_time_prediction_error_us: histogram,
    #[describe("Distribution of predicted execution times from metering service (microseconds)")]
    tx_predicted_execution_time_us: histogram,
    #[describe("Distribution of actual execution times (microseconds)")]
    tx_actual_execution_time_us: histogram,
    #[describe("Per-transaction state root gas (computed from metering data)")]
    tx_state_root_gas: histogram,
    #[describe("Cumulative state root gas per block")]
    block_state_root_gas: histogram,
    #[describe("Distribution of predicted state root times from metering service (microseconds)")]
    tx_predicted_state_root_time_us: histogram,
    #[describe("Ratio of state_root_time_us / gas_used for each transaction")]
    state_root_time_per_gas_ratio: histogram,
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
    #[describe("Flashblock execution time used in microseconds")]
    #[label(flashblock_index)]
    flashblock_execution_time_used_us: histogram,
    #[describe("Flashblock execution time headroom in microseconds")]
    #[label(flashblock_index)]
    flashblock_execution_time_headroom_us: histogram,
    #[describe("Flashblock cumulative state root gas used")]
    #[label(flashblock_index)]
    flashblock_state_root_gas_used: histogram,
    #[describe("Flashblock state root gas headroom")]
    #[label(flashblock_index)]
    flashblock_state_root_gas_headroom: histogram,
    #[describe("Priority fee of rejected transactions")]
    #[label(reason)]
    rejected_tx_priority_fee: histogram,
    #[describe("Actual execution time for transactions without metering data (microseconds)")]
    unmetered_tx_actual_execution_time_us: histogram,
    #[describe("Transactions committed to a payload without metering data")]
    metering_late_arrival_total: counter,
    #[describe("Time between first metering lookup miss and metering data arrival (milliseconds)")]
    metering_late_arrival_latency_ms: histogram,
    #[describe("Execution time from late-arriving metering data (microseconds)")]
    metering_late_arrival_execution_time_us: histogram,
    #[describe("State root time from late-arriving metering data (microseconds)")]
    metering_late_arrival_state_root_time_us: histogram,
    #[describe("Number of accounts modified by a transaction (from EVM post-state)")]
    tx_accounts_modified: histogram,
    #[describe("Number of storage slots modified by a transaction (from EVM post-state)")]
    tx_storage_slots_modified: histogram,
}

impl BuilderMetrics {
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

        Self::flashblock_execution_time_used_us(flashblock_index.clone())
            .record(info.flashblock_execution_time_us as f64);
        if let Some(flashblock_execution_time_limit_us) = limits.flashblock_execution_time_limit_us
        {
            Self::flashblock_execution_time_headroom_us(flashblock_index.clone()).record(
                flashblock_execution_time_limit_us.saturating_sub(info.flashblock_execution_time_us)
                    as f64,
            );
        }

        Self::flashblock_state_root_gas_used(flashblock_index.clone())
            .record(info.cumulative_state_root_gas as f64);
        if let Some(block_state_root_gas_limit) = limits.block_state_root_gas_limit {
            Self::flashblock_state_root_gas_headroom(flashblock_index.clone())
                .record(block_state_root_gas_limit.saturating_sub(info.cumulative_state_root_gas)
                    as f64);
        }

        for (reason, count) in diag.rejection_counts() {
            if count == 0 {
                continue;
            }
            Self::flashblock_rejections_total(flashblock_index.clone(), reason).increment(count);
        }
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
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    #[test]
    fn record_flashblock_diagnostics_emits_labeled_metrics() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let diag = FlashblockDiagnostics {
            txs_considered: 6,
            txs_included: 3,
            txs_rejected_gas: 2,
            txs_rejected_da: 1,
            txs_rejected_metering_data_pending: 1,
            min_priority_fee: Some(200_000),
            ..Default::default()
        };
        let info = ExecutionInfo {
            cumulative_gas_used: 60,
            cumulative_da_bytes_used: 15,
            flashblock_execution_time_us: 40,
            cumulative_state_root_gas: 90,
            ..Default::default()
        };
        let limits = ResourceLimits {
            block_gas_limit: 100,
            block_data_limit: Some(20),
            flashblock_execution_time_limit_us: Some(50),
            block_state_root_gas_limit: Some(100),
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
            rendered
                .contains("base_builder_flashblock_gas_headroom_sum{flashblock_index=\"7\"} 40")
        );
        assert!(rendered.contains(
            "base_builder_flashblock_min_priority_fee_above_threshold_total{flashblock_index=\"7\",threshold=\"100wei\"} 1"
        ));
    }
}
