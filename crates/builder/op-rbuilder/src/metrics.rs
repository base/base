use reth_metrics::{metrics::Counter, metrics::Gauge, metrics::Histogram, Metrics};

/// op-rbuilder metrics
#[derive(Metrics, Clone)]
#[metrics(scope = "op_rbuilder")]
pub struct OpRBuilderMetrics {
    /// Number of builder landed blocks
    pub builder_landed_blocks: Gauge,
    /// Last built block height
    pub last_landed_block_height: Gauge,
    /// Number of blocks the builder did not land
    pub builder_landed_blocks_missed: Gauge,
    /// Block built success
    pub block_built_success: Counter,
    /// Total duration of building a block
    pub total_block_built_duration: Histogram,
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

impl OpRBuilderMetrics {
    pub fn inc_builder_landed_blocks(&self) {
        self.builder_landed_blocks.increment(1);
    }

    pub fn dec_builder_landed_blocks(&self) {
        self.builder_landed_blocks.decrement(1);
    }

    pub fn inc_builder_landed_blocks_missed(&self) {
        self.builder_landed_blocks_missed.increment(1);
    }

    pub fn set_last_landed_block_height(&self, height: u64) {
        self.last_landed_block_height.set(height as f64);
    }
}
