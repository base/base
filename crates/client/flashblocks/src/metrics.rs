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

    /// Time spent on parallel sender recovery (ECDSA operations).
    #[metric(describe = "Time spent on parallel sender recovery")]
    pub sender_recovery_duration: Histogram,

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

    // RPC metrics
    /// Count of times flashblocks get_transaction_count is called.
    #[metric(describe = "Count of times flashblocks get_transaction_count is called")]
    pub rpc_get_transaction_count: Counter,

    /// Count of times flashblocks get_transaction_receipt is called.
    #[metric(describe = "Count of times flashblocks get_transaction_receipt is called")]
    pub rpc_get_transaction_receipt: Counter,

    /// Count of times flashblocks get_transaction_by_hash is called.
    #[metric(describe = "Count of times flashblocks get_transaction_by_hash is called")]
    pub rpc_get_transaction_by_hash: Counter,

    /// Count of times flashblocks get_balance is called.
    #[metric(describe = "Count of times flashblocks get_balance is called")]
    pub rpc_get_balance: Counter,

    /// Count of times flashblocks get_block_by_number is called.
    #[metric(describe = "Count of times flashblocks get_block_by_number is called")]
    pub rpc_get_block_by_number: Counter,

    /// Count of times flashblocks call is called.
    #[metric(describe = "Count of times flashblocks call is called")]
    pub rpc_call: Counter,

    /// Count of times flashblocks estimate_gas is called.
    #[metric(describe = "Count of times flashblocks estimate_gas is called")]
    pub rpc_estimate_gas: Counter,

    /// Count of times flashblocks simulate_v1 is called.
    #[metric(describe = "Count of times flashblocks simulate_v1 is called")]
    pub rpc_simulate_v1: Counter,

    /// Count of times flashblocks get_logs is called.
    #[metric(describe = "Count of times flashblocks get_logs is called")]
    pub rpc_get_logs: Counter,
}
