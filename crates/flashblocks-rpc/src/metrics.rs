use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;

#[derive(Metrics, Clone)]
#[metrics(scope = "reth_flashblocks")]
pub struct Metrics {
    // ── existing ──────────────────────────────────────────────────────────────
    #[metric(describe = "Count of times upstream receiver was closed/errored")]
    pub upstream_errors: Counter,

    #[metric(describe = "Count of messages received from the upstream source")]
    pub upstream_messages: Gauge,

    #[metric(describe = "Time taken to process a message")]
    pub block_processing_duration: Histogram,

    #[metric(describe = "Number of Flashblocks that arrive in an unexpected order")]
    pub unexpected_block_order: Counter,

    #[metric(describe = "Count of times flashblocks get_transaction_count is called")]
    pub get_transaction_count: Counter,

    #[metric(describe = "Count of times flashblocks get_transaction_receipt is called")]
    pub get_transaction_receipt: Counter,

    #[metric(describe = "Count of times flashblocks get_balance is called")]
    pub get_balance: Counter,

    #[metric(describe = "Count of times flashblocks get_block_by_number is called")]
    pub get_block_by_number: Counter,

    #[metric(describe = "Number of flashblocks in a block")]
    pub flashblocks_in_block: Histogram,

    #[metric(describe = "Count of times flashblocks are unable to be converted to blocks")]
    pub block_processing_error: Counter,

    #[metric(describe = "Count of times flashblocks call is called")]
    pub call: Counter,

    // ── new: pending snapshot lifecycle ───────────────────────────────────────
    #[metric(describe = "Number of times a pending snapshot was installed")]
    pub pending_set: Counter,

    #[metric(describe = "Number of times an existing pending snapshot was replaced")]
    pub pending_replaced: Counter,

    #[metric(describe = "Number of times pending snapshot was cleared (any reason)")]
    pub pending_cleared: Counter,

    #[metric(describe = "Number of times clear was a no-op (nothing to clear)")]
    pub pending_clear_noop: Counter,

    #[metric(
        describe = "Number of times pending snapshot was cleared because canonical caught up"
    )]
    pub pending_clear_catchup: Counter,

    #[metric(describe = "Pending snapshot block number (current)")]
    pub pending_snapshot_height: Gauge,

    #[metric(describe = "Pending snapshot flashblock index (current)")]
    pub pending_snapshot_fb_index: Gauge,

    #[metric(describe = "Distance between canonical head and pending height at catch-up")]
    pub catchup_distance: Histogram,
}
