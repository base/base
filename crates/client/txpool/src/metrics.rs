//! Metrics for transaction tracing.

use metrics::Histogram;
use metrics_derive::Metrics;

/// Metrics for the `reth_transaction_tracing` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_transaction_tracing")]
pub struct Metrics {
    /// Time taken for a transaction to be included in a block.
    #[metric(describe = "Time taken for a transaction to be included in a block")]
    pub transaction_inclusion_duration: Histogram,
}
