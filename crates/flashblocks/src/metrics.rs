use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;
/// Metrics for the `reth_flashblocks` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
/// - Counters are monotonic event counts.
/// - Gauges reflect the current value/state.
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_flashblocks")]
pub(crate) struct Metrics {
    #[metric(describe = "Count of times upstream receiver was closed/errored")]
    pub upstream_errors: Counter,

    #[metric(describe = "Count of messages received from the upstream source")]
    pub upstream_messages: Counter,

    #[metric(describe = "Time taken to process a message")]
    pub block_processing_duration: Histogram,

    #[metric(describe = "Number of Flashblocks that arrive in an unexpected order")]
    pub unexpected_block_order: Counter,

    #[metric(describe = "Number of flashblocks in a block")]
    pub flashblocks_in_block: Histogram,

    #[metric(describe = "Count of times flashblocks are unable to be converted to blocks")]
    pub block_processing_error: Counter,

    #[metric(
        describe = "Number of times pending snapshot was cleared because canonical caught up"
    )]
    pub pending_clear_catchup: Counter,

    #[metric(describe = "Number of times pending snapshot was cleared because of reorg")]
    pub pending_clear_reorg: Counter,

    #[metric(describe = "Pending snapshot flashblock index (current)")]
    pub pending_snapshot_fb_index: Gauge,

    #[metric(describe = "Pending snapshot block number (current)")]
    pub pending_snapshot_height: Gauge,

    #[metric(describe = "Total number of WebSocket reconnection attempts")]
    pub reconnect_attempts: Counter,
}
