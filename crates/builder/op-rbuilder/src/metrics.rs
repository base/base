use reth_metrics::{metrics::Gauge, Metrics};

/// op-rbuilder metrics
#[derive(Metrics)]
#[metrics(scope = "op_rbuilder")]
pub struct OpRBuilderMetrics {
    /// Number of builder built blocks
    pub builder_built_blocks: Gauge,
    /// Last built block height
    pub last_built_block_height: Gauge,
}

impl OpRBuilderMetrics {
    pub fn inc_builder_built_blocks(&self) {
        self.builder_built_blocks.increment(1);
    }

    pub fn dec_builder_built_blocks(&self) {
        self.builder_built_blocks.decrement(1);
    }

    pub fn set_last_built_block_height(&self, height: u64) {
        self.last_built_block_height.set(height as f64);
    }
}
