use metrics::IntoF64;
use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

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
    /// Number of messages sent
    pub messages_sent_count: Counter,
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
    /// Count of the number of times we were unable to resolve metering information due to locking
    pub metering_locked_transaction: Counter,

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
