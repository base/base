use metrics::Counter;
use metrics_derive::Metrics;
/// Metrics for the `base_reth_rpc` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
/// - Counters are monotonic event counts.
/// - Gauges reflect the current value/state.
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_flashblocks")]
pub(crate) struct Metrics {
    #[metric(describe = "Count of times flashblocks get_transaction_count is called")]
    pub get_transaction_count: Counter,

    #[metric(describe = "Count of times flashblocks get_transaction_receipt is called")]
    pub get_transaction_receipt: Counter,

    #[metric(describe = "Count of times flashblocks get_transaction_by_hash is called")]
    pub get_transaction_by_hash: Counter,

    #[metric(describe = "Count of times flashblocks get_balance is called")]
    pub get_balance: Counter,

    #[metric(describe = "Count of times flashblocks get_block_by_number is called")]
    pub get_block_by_number: Counter,

    #[metric(describe = "Count of times flashblocks call is called")]
    pub call: Counter,

    #[metric(describe = "Count of times flashblocks estimate_gas is called")]
    pub estimate_gas: Counter,

    #[metric(describe = "Count of times flashblocks simulate_v1 is called")]
    pub simulate_v1: Counter,

    #[metric(describe = "Count of times flashblocks get_logs is called")]
    pub get_logs: Counter,
}
