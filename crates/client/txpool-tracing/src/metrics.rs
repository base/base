//! Metrics for transaction tracing.

use metrics::Histogram;
use metrics_derive::Metrics;

/// Metrics for the `reth_transaction_tracing` component.
/// Conventions:
/// - Durations are recorded in seconds (histograms).
#[derive(Metrics, Clone)]
#[metrics(scope = "reth_transaction_tracing")]
pub struct Metrics {
    /// Time taken for a transaction to be included in a block from when it's marked as pending.
    #[metric(
        describe = "Time taken for a transaction to be included in a block from when it's marked as pending"
    )]
    pub inclusion_duration: Histogram,
    /// Time taken for a transaction to be included in a flashblock from when it's marked as pending.
    #[metric(
        describe = "Time taken for a transaction to be included in a flashblock from when it's marked as pending"
    )]
    pub fb_inclusion_duration: Histogram,
}
