use metrics::{Counter, Histogram};
use metrics_derive::Metrics;
#[derive(Metrics, Clone)]
#[metrics(scope = "flashblocks_archiver")]
pub struct Metrics {
    #[metric(describe = "Time taken to store a batch of flashblocks in the database")]
    pub store_flashblock_duration: Histogram,

    #[metric(describe = "Count of errors when flushing a batch of flashblocks")]
    pub flush_batch_error: Counter,
}
