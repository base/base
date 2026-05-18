//! Metrics for the transaction forwarder.

use jsonrpsee::core::ClientError;

base_metrics::define_metrics! {
    txpool.forwarder,
    struct = ForwarderMetrics,
    #[describe("Total RPC batches sent successfully")]
    #[label(builder_url)]
    batches_sent: counter,
    #[describe("Total individual transactions forwarded")]
    #[label(builder_url)]
    txs_forwarded: counter,
    #[describe("Total RPC send errors (after all retries exhausted)")]
    #[label(builder_url)]
    #[label(reason)]
    rpc_errors: counter,
    #[describe("Total number of transactions rejected by the builder's pool within successful batch calls")]
    #[label(builder_url)]
    num_tx_rejected_in_batch: counter,
    #[describe("Total lag events from the broadcast receiver")]
    #[label(builder_url)]
    batches_lagged: counter,
    #[describe("Total individual transactions skipped due to lag")]
    #[label(builder_url)]
    txs_lagged: counter,
    #[describe("RPC round-trip latency in seconds (including retries)")]
    #[label(builder_url)]
    rpc_latency: histogram,
    #[describe("Current number of transactions buffered and awaiting send")]
    #[label(builder_url)]
    buffer_size: gauge,
}

impl ForwarderMetrics {
    /// Maps a [`ClientError`] to a static label for the `rpc_errors` metric.
    pub const fn rpc_error_label(err: &ClientError) -> &'static str {
        match err {
            ClientError::Transport(_) => "transport",
            ClientError::RequestTimeout => "request_timeout",
            ClientError::RestartNeeded(_) => "restart_needed",
            ClientError::Call(_) => "call_error",
            ClientError::ParseError(_) => "parse_error",
            _ => "other",
        }
    }
}
