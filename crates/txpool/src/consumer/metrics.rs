use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge},
};

/// Prometheus metrics for the transaction consumer.
#[derive(Metrics, Clone)]
#[metrics(scope = "txpool.consumer")]
pub struct ConsumerMetrics {
    /// Total transactions read from the pool iterator.
    pub txs_read: Counter,
    /// Total transactions sent to the channel after deduplication.
    pub txs_sent: Counter,
    /// Total transactions skipped by the validator.
    pub txs_ignored: Counter,
    /// Current number of entries in the dedup cache.
    pub dedup_cache_size: Gauge,
}
