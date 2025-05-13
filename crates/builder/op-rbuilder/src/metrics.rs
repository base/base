use reth_metrics::{
    metrics::{Counter, Histogram},
    Metrics,
};

/// op-rbuilder metrics
#[derive(Metrics, Clone)]
#[metrics(scope = "op_rbuilder")]
pub struct OpRBuilderMetrics {
    /// Block built success
    pub block_built_success: Counter,
    /// Number of flashblocks added to block (Total per block)
    #[cfg(feature = "flashblocks")]
    pub flashblock_count: Histogram,
    /// Number of messages sent
    #[cfg(feature = "flashblocks")]
    pub messages_sent_count: Counter,
    /// Total duration of building a block
    pub total_block_built_duration: Histogram,
    /// Flashblock build duration
    #[cfg(feature = "flashblocks")]
    pub flashblock_build_duration: Histogram,
    /// Number of invalid blocks
    #[cfg(feature = "flashblocks")]
    pub invalid_blocks_count: Counter,
    /// Duration of fetching transactions from the pool
    pub transaction_pool_fetch_duration: Histogram,
    /// Duration of state root calculation
    pub state_root_calculation_duration: Histogram,
    /// Duration of sequencer transaction execution
    pub sequencer_tx_duration: Histogram,
    /// Duration of state merge transitions
    pub state_transition_merge_duration: Histogram,
    /// Duration of payload simulation of all transactions
    pub payload_tx_simulation_duration: Histogram,
    /// Number of transaction considered for inclusion in the block
    pub payload_num_tx_considered: Histogram,
    /// Payload byte size
    pub payload_byte_size: Histogram,
    /// Number of transactions in the payload
    pub payload_num_tx: Histogram,
    /// Number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated: Histogram,
    /// Number of transactions in the payload that were successfully simulated
    pub payload_num_tx_simulated_success: Histogram,
    /// Number of transactions in the payload that failed simulation
    pub payload_num_tx_simulated_fail: Histogram,
    /// Duration of tx simulation
    pub tx_simulation_duration: Histogram,
    /// Byte size of transactions
    pub tx_byte_size: Histogram,
}
