//! Metrics for flashblocks.

use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;

/// Metrics for the `reth_flashblocks` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
/// - Counters are monotonic event counts.
/// - Gauges reflect the current value/state.
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_flashblocks")]
pub struct Metrics {
    /// Count of times upstream receiver was closed/errored.
    #[metric(describe = "Count of times upstream receiver was closed/errored")]
    pub upstream_errors: Counter,

    /// Count of messages received from the upstream source.
    #[metric(describe = "Count of messages received from the upstream source")]
    pub upstream_messages: Counter,

    /// Time taken to process a message.
    #[metric(describe = "Time taken to process a message")]
    pub block_processing_duration: Histogram,

    /// Number of Flashblocks that arrive in an unexpected order.
    #[metric(describe = "Number of Flashblocks that arrive in an unexpected order")]
    pub unexpected_block_order: Counter,

    /// Number of flashblocks contained within a single block.
    #[metric(describe = "Number of flashblocks in a block")]
    pub flashblocks_in_block: Histogram,

    /// Count of times flashblocks are unable to be converted to blocks.
    #[metric(describe = "Count of times flashblocks are unable to be converted to blocks")]
    pub block_processing_error: Counter,

    /// Count of times pending snapshot was cleared because canonical caught up.
    #[metric(
        describe = "Number of times pending snapshot was cleared because canonical caught up"
    )]
    pub pending_clear_catchup: Counter,

    /// Number of times pending snapshot was cleared because of reorg.
    #[metric(describe = "Number of times pending snapshot was cleared because of reorg")]
    pub pending_clear_reorg: Counter,

    /// Pending snapshot flashblock index (current).
    #[metric(describe = "Pending snapshot flashblock index (current)")]
    pub pending_snapshot_fb_index: Gauge,

    /// Pending snapshot block number (current).
    #[metric(describe = "Pending snapshot block number (current)")]
    pub pending_snapshot_height: Gauge,

    /// Total number of WebSocket reconnection attempts.
    #[metric(describe = "Total number of WebSocket reconnection attempts")]
    pub reconnect_attempts: Counter,

    /// Time taken to clone bundle state.
    #[metric(describe = "Time taken to clone bundle state")]
    pub bundle_state_clone_duration: Histogram,

    /// Size of bundle state being cloned (number of accounts).
    #[metric(describe = "Size of bundle state being cloned (number of accounts)")]
    pub bundle_state_clone_size: Histogram,
}
