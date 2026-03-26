use metrics::{IntoF64, counter, histogram};
use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

use crate::{ExecutionInfo, FlashblockDiagnostics, ResourceLimits};

const FLASHBLOCK_INDEX_LABEL: &str = "flashblock_index";
const OUTCOME_LABEL: &str = "outcome";
const REASON_LABEL: &str = "reason";
const THRESHOLD_LABEL: &str = "threshold";

const FLASHBLOCK_SELECTION_TOTAL: &str = "base_builder_flashblock_selection_total";
const FLASHBLOCK_MIN_PRIORITY_FEE_ABOVE_THRESHOLD_TOTAL: &str =
    "base_builder_flashblock_min_priority_fee_above_threshold_total";
const FLASHBLOCK_TXS_CONSIDERED: &str = "base_builder_flashblock_txs_considered";
const FLASHBLOCK_REJECTIONS_TOTAL: &str = "base_builder_flashblock_rejections_total";
const FLASHBLOCK_TXS_INCLUDED: &str = "base_builder_flashblock_txs_included";
const FLASHBLOCK_TXS_REJECTED: &str = "base_builder_flashblock_txs_rejected";
const FLASHBLOCK_MIN_PRIORITY_FEE_WEI: &str = "base_builder_flashblock_min_priority_fee_wei";
const FLASHBLOCK_GAS_HEADROOM: &str = "base_builder_flashblock_gas_headroom";
const FLASHBLOCK_GAS_HEADROOM_PCT: &str = "base_builder_flashblock_gas_headroom_pct";
const FLASHBLOCK_DA_BYTES_USED: &str = "base_builder_flashblock_da_bytes_used";
const FLASHBLOCK_DA_HEADROOM_BYTES: &str = "base_builder_flashblock_da_headroom_bytes";
const FLASHBLOCK_EXECUTION_TIME_USED_US: &str = "base_builder_flashblock_execution_time_used_us";
const FLASHBLOCK_EXECUTION_TIME_HEADROOM_US: &str =
    "base_builder_flashblock_execution_time_headroom_us";
const FLASHBLOCK_STATE_ROOT_TIME_USED_US: &str = "base_builder_flashblock_state_root_time_used_us";
const FLASHBLOCK_STATE_ROOT_TIME_HEADROOM_US: &str =
    "base_builder_flashblock_state_root_time_headroom_us";
const PRIORITY_FEE_THRESHOLDS_WEI: [(&str, u64); 3] =
    [("100wei", 100), ("100kwei", 100_000), ("1mwei", 1_000_000)];

/// base-builder metrics
#[derive(Metrics, Clone)]
#[metrics(scope = "base_builder")]
pub struct BuilderMetrics {
    /// Block built success
    pub block_built_success: Counter,
    /// Block synced success
    pub block_synced_success: Counter,
    /// Number of flashblocks added to block (Total per block)
    pub flashblock_count: Histogram,
    /// Histogram of the time taken to build a block
    pub total_block_built_duration: Histogram,
    /// Latest time taken to build a block
    pub total_block_built_gauge: Gauge,
    /// Histogram of the time taken to build a Flashblock
    pub flashblock_build_duration: Histogram,
    /// Histogram of the time taken to sync a Flashblock
    pub flashblock_sync_duration: Histogram,
    /// Flashblock UTF8 payload byte size histogram
    pub flashblock_byte_size_histogram: Histogram,
    /// Histogram of transactions in a Flashblock
    pub flashblock_num_tx_histogram: Histogram,
    /// Number of invalid blocks
    pub invalid_built_blocks_count: Counter,
    /// Number of invalid synced blocks
    pub invalid_synced_blocks_count: Counter,
    /// Histogram of fetching transactions from the pool duration
    pub transaction_pool_fetch_duration: Histogram,
    /// Latest time taken to fetch tx from the pool
    pub transaction_pool_fetch_gauge: Gauge,
    /// Histogram of state root calculation duration
    pub state_root_calculation_duration: Histogram,
    /// Latest state root calculation duration
    pub state_root_calculation_gauge: Gauge,
    /// Histogram of sequencer transaction execution duration
    pub sequencer_tx_duration: Histogram,
    /// Latest sequencer transaction execution duration
    pub sequencer_tx_gauge: Gauge,
    /// Histogram of state merge transitions duration
    pub state_transition_merge_duration: Histogram,
    /// Latest state merge transitions duration
    pub state_transition_merge_gauge: Gauge,
    /// Histogram of the duration of payload simulation of all transactions
    pub payload_transaction_simulation_duration: Histogram,
    /// Latest payload simulation of all transactions duration
    pub payload_transaction_simulation_gauge: Gauge,
    /// Number of transaction considered for inclusion in the block
    pub payload_num_tx_considered: Histogram,
    /// Latest number of transactions considered for inclusion in the block
    pub payload_num_tx_considered_gauge: Gauge,
    /// Payload byte size histogram
    pub payload_byte_size: Histogram,
    /// Latest Payload byte size
    pub payload_byte_size_gauge: Gauge,
    /// Histogram of transactions in the payload
    pub payload_num_tx: Histogram,
    /// Latest number of transactions in the payload
    pub payload_num_tx_gauge: Gauge,
    /// Histogram of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated: Histogram,
    /// Latest number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_gauge: Gauge,
    /// Histogram of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success: Histogram,
    /// Latest number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success_gauge: Gauge,
    /// Histogram of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail: Histogram,
    /// Latest number of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail_gauge: Gauge,
    /// Histogram of gas used by successful transactions
    pub successful_tx_gas_used: Histogram,
    /// Histogram of gas used by reverted transactions
    pub reverted_tx_gas_used: Histogram,
    /// Gas used by reverted transactions in the latest block
    pub payload_reverted_tx_gas_used: Gauge,
    /// Histogram of tx simulation duration
    pub tx_simulation_duration: Histogram,
    /// Byte size of transactions
    pub tx_byte_size: Histogram,
    /// How much less flashblocks we issue to be on time with block construction
    pub reduced_flashblocks_number: Histogram,
    /// How much less flashblocks we issued in reality, comparing to calculated number for block
    pub missing_flashblocks_count: Histogram,
    /// How much time we have deducted from block building time
    pub flashblocks_time_drift: Histogram,
    /// Time offset we used for first flashblock
    pub first_flashblock_time_offset: Histogram,
    /// Count of the number of times transactions had metering information
    pub metering_known_transaction: Counter,
    /// Count of the number of times transactions did not have any metering information
    pub metering_unknown_transaction: Counter,
    /// Number of LRU evictions from `MeteringStore`
    pub metering_store_lru_evictions: Counter,

    // === DA Size Limit Metrics (always enforced, operator-configured) ===
    /// Transactions rejected by per-tx DA size limit
    pub tx_da_size_exceeded_total: Counter,
    /// Transactions rejected by block DA size limit
    pub block_da_size_exceeded_total: Counter,

    // === Protocol-Enforced Limit Metrics ===
    /// Transactions rejected by DA footprint limit (post-Jovian, protocol-enforced)
    pub da_footprint_exceeded_total: Counter,
    /// Transactions rejected by gas limit (protocol-enforced)
    pub gas_limit_exceeded_total: Counter,

    // === Block Uncompressed Size Limit Metrics ===
    /// Transactions rejected by block uncompressed size limit
    pub block_uncompressed_size_exceeded_total: Counter,
    /// Cumulative uncompressed block size at end of block
    pub block_uncompressed_size: Histogram,

    // === Execution Metering Limit Metrics (metering-service-dependent) ===
    /// Transactions that would be rejected by execution metering limits
    pub resource_limit_would_reject_total: Counter,
    /// Transactions that exceeded per-tx execution time limit
    pub tx_execution_time_exceeded_total: Counter,
    /// Transactions that exceeded flashblock execution time budget
    pub flashblock_execution_time_exceeded_total: Counter,
    /// Transactions that exceeded per-tx state root time limit
    pub tx_state_root_time_exceeded_total: Counter,
    /// Transactions that exceeded block state root time budget
    pub block_state_root_time_exceeded_total: Counter,

    // === Execution Time Prediction Accuracy ===
    /// Histogram of (predicted - actual) execution time per transaction in microseconds.
    pub execution_time_prediction_error_us: Histogram,
    /// Distribution of predicted execution times from metering service (microseconds)
    pub tx_predicted_execution_time_us: Histogram,
    /// Distribution of actual execution times (microseconds)
    pub tx_actual_execution_time_us: Histogram,

    // === State Root Time Prediction Distribution ===
    /// Distribution of predicted state root times from metering service (microseconds)
    pub tx_predicted_state_root_time_us: Histogram,
    /// Cumulative predicted state root time per block (microseconds)
    pub block_predicted_state_root_time_us: Histogram,

    // === State Root Time / Gas Ratio (Anomaly Detection) ===
    /// Ratio of `state_root_time_us` / `gas_used` for each transaction.
    pub state_root_time_per_gas_ratio: Histogram,
}

impl BuilderMetrics {
    /// Records per-flashblock selection diagnostics as labeled metrics.
    pub fn record_flashblock_diagnostics(
        &self,
        flashblock_index: u64,
        diag: &FlashblockDiagnostics,
        info: &ExecutionInfo,
        limits: &ResourceLimits,
    ) {
        let flashblock_index = flashblock_index.to_string();
        counter!(
            FLASHBLOCK_SELECTION_TOTAL,
            FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
            OUTCOME_LABEL => diag.selection_outcome().as_str(),
        )
        .increment(1);

        histogram!(FLASHBLOCK_TXS_CONSIDERED, FLASHBLOCK_INDEX_LABEL => flashblock_index.clone())
            .record(diag.txs_considered as f64);
        histogram!(FLASHBLOCK_TXS_INCLUDED, FLASHBLOCK_INDEX_LABEL => flashblock_index.clone())
            .record(diag.txs_included as f64);
        histogram!(FLASHBLOCK_TXS_REJECTED, FLASHBLOCK_INDEX_LABEL => flashblock_index.clone())
            .record(diag.txs_rejected_total() as f64);

        if let Some(min_priority_fee) = diag.min_priority_fee {
            histogram!(
                FLASHBLOCK_MIN_PRIORITY_FEE_WEI,
                FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
            )
            .record(min_priority_fee as f64);
            for (threshold, threshold_wei) in PRIORITY_FEE_THRESHOLDS_WEI {
                if min_priority_fee > threshold_wei {
                    counter!(
                        FLASHBLOCK_MIN_PRIORITY_FEE_ABOVE_THRESHOLD_TOTAL,
                        FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
                        THRESHOLD_LABEL => threshold,
                    )
                    .increment(1);
                }
            }
        }

        let gas_headroom = limits.block_gas_limit.saturating_sub(info.cumulative_gas_used);
        histogram!(FLASHBLOCK_GAS_HEADROOM, FLASHBLOCK_INDEX_LABEL => flashblock_index.clone())
            .record(gas_headroom as f64);
        if limits.block_gas_limit > 0 {
            histogram!(
                FLASHBLOCK_GAS_HEADROOM_PCT,
                FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
            )
            .record(gas_headroom as f64 / limits.block_gas_limit as f64 * 100.0);
        }

        histogram!(FLASHBLOCK_DA_BYTES_USED, FLASHBLOCK_INDEX_LABEL => flashblock_index.clone())
            .record(info.cumulative_da_bytes_used as f64);
        if let Some(block_data_limit) = limits.block_data_limit {
            histogram!(
                FLASHBLOCK_DA_HEADROOM_BYTES,
                FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
            )
            .record(block_data_limit.saturating_sub(info.cumulative_da_bytes_used) as f64);
        }

        histogram!(
            FLASHBLOCK_EXECUTION_TIME_USED_US,
            FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
        )
        .record(info.flashblock_execution_time_us as f64);
        if let Some(flashblock_execution_time_limit_us) = limits.flashblock_execution_time_limit_us
        {
            histogram!(
                FLASHBLOCK_EXECUTION_TIME_HEADROOM_US,
                FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
            )
            .record(
                flashblock_execution_time_limit_us.saturating_sub(info.flashblock_execution_time_us)
                    as f64,
            );
        }

        histogram!(
            FLASHBLOCK_STATE_ROOT_TIME_USED_US,
            FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
        )
        .record(info.cumulative_state_root_time_us as f64);
        if let Some(block_state_root_time_limit_us) = limits.block_state_root_time_limit_us {
            histogram!(
                FLASHBLOCK_STATE_ROOT_TIME_HEADROOM_US,
                FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
            )
            .record(
                block_state_root_time_limit_us.saturating_sub(info.cumulative_state_root_time_us)
                    as f64,
            );
        }

        for (reason, count) in diag.rejection_counts() {
            if count == 0 {
                continue;
            }
            counter!(
                FLASHBLOCK_REJECTIONS_TOTAL,
                FLASHBLOCK_INDEX_LABEL => flashblock_index.clone(),
                REASON_LABEL => reason,
            )
            .increment(count);
        }
    }

    pub fn set_payload_builder_metrics(
        &self,
        payload_transaction_simulation_time: impl IntoF64 + Copy,
        num_txs_considered: impl IntoF64 + Copy,
        num_txs_simulated: impl IntoF64 + Copy,
        num_txs_simulated_success: impl IntoF64 + Copy,
        num_txs_simulated_fail: impl IntoF64 + Copy,
        reverted_gas_used: impl IntoF64,
    ) {
        self.payload_transaction_simulation_duration.record(payload_transaction_simulation_time);
        self.payload_transaction_simulation_gauge.set(payload_transaction_simulation_time);
        self.payload_num_tx_considered.record(num_txs_considered);
        self.payload_num_tx_considered_gauge.set(num_txs_considered);
        self.payload_num_tx_simulated.record(num_txs_simulated);
        self.payload_num_tx_simulated_gauge.set(num_txs_simulated);
        self.payload_num_tx_simulated_success.record(num_txs_simulated_success);
        self.payload_num_tx_simulated_success_gauge.set(num_txs_simulated_success);
        self.payload_num_tx_simulated_fail.record(num_txs_simulated_fail);
        self.payload_num_tx_simulated_fail_gauge.set(num_txs_simulated_fail);
        self.payload_reverted_tx_gas_used.set(reverted_gas_used);
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
        let metrics = BuilderMetrics::default();
        let diag = FlashblockDiagnostics {
            txs_considered: 6,
            txs_included: 3,
            txs_rejected_gas: 2,
            txs_rejected_da: 1,
            min_priority_fee: Some(200_000),
            ..Default::default()
        };
        let info = ExecutionInfo {
            cumulative_gas_used: 60,
            cumulative_da_bytes_used: 15,
            flashblock_execution_time_us: 40,
            cumulative_state_root_time_us: 90,
            ..Default::default()
        };
        let limits = ResourceLimits {
            block_gas_limit: 100,
            block_data_limit: Some(20),
            flashblock_execution_time_limit_us: Some(50),
            block_state_root_time_limit_us: Some(100),
            ..Default::default()
        };

        metrics::with_local_recorder(&recorder, || {
            metrics.record_flashblock_diagnostics(7, &diag, &info, &limits);
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
