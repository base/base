use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_flashblocks")]
pub struct Metrics {
    #[metric(describe = "Count of times upstream receiver was closed/errored")]
    pub upstream_errors: Counter,

    #[metric(describe = "Count of messages received from the upstream source")]
    pub upstream_messages: Gauge,

    #[metric(describe = "Time taken to process a message")]
    pub block_processing_duration: Histogram,

    #[metric(describe = "Incremented when a flashblock is created and reset to 0 when new block is processed")]
    pub flashblock_created: Gauge,

    #[metric(describe = "Number of flashblocks in a block")]
    pub flashblocks_per_block: Histogram,
}
