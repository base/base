use metrics::{Counter, Histogram, Label, counter, histogram};

/// Prometheus metrics for a single forwarder instance, labeled by builder URL.
#[derive(Clone)]
pub struct ForwarderMetrics {
    /// Total RPC batches sent successfully.
    pub batches_sent: Counter,
    /// Total individual transactions forwarded.
    pub txs_forwarded: Counter,
    /// Total RPC send errors (after all retries exhausted).
    pub rpc_errors: Counter,
    /// Total lag events from the broadcast receiver.
    pub batches_lagged: Counter,
    /// Total individual transactions skipped due to lag.
    pub txs_lagged: Counter,
    /// RPC round-trip latency in seconds (including retries).
    pub rpc_latency: Histogram,
}

impl ForwarderMetrics {
    /// Creates metrics labeled with the given builder URL.
    pub fn new(builder_url: &str) -> Self {
        let labels = vec![Label::new("builder_url", builder_url.to_string())];
        Self {
            batches_sent: counter!("txpool.forwarder.batches_sent", labels.clone()),
            txs_forwarded: counter!("txpool.forwarder.txs_forwarded", labels.clone()),
            rpc_errors: counter!("txpool.forwarder.rpc_errors", labels.clone()),
            batches_lagged: counter!("txpool.forwarder.batches_lagged", labels.clone()),
            txs_lagged: counter!("txpool.forwarder.txs_lagged", labels.clone()),
            rpc_latency: histogram!("txpool.forwarder.rpc_latency", labels),
        }
    }
}

impl std::fmt::Debug for ForwarderMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwarderMetrics").finish_non_exhaustive()
    }
}
