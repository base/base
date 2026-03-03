use reth_metrics::{
    Metrics,
    metrics::{Counter, Histogram},
};

/// Prometheus metrics for a single forwarder instance.
#[derive(Metrics, Clone)]
#[metrics(scope = "txpool.forwarder")]
pub struct ForwarderMetrics {
    /// Total RPC batches sent successfully.
    pub batches_sent: Counter,
    /// Total individual transactions forwarded.
    pub txs_forwarded: Counter,
    /// Total RPC send errors (after all retries exhausted).
    pub rpc_errors: Counter,
    /// Total batches dropped due to lagged broadcast receiver.
    pub batches_lagged: Counter,
    /// RPC round-trip latency in seconds.
    pub rpc_latency: Histogram,
}
