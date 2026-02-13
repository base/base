use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::tracker::TransactionTracker;

/// Aggregated load test results.
#[derive(Debug, Serialize, Deserialize)]
pub struct TestResults {
    /// Test configuration used.
    pub config: TestConfig,
    /// Throughput measurements.
    pub results: ThroughputResults,
    /// Error counts.
    pub errors: ErrorResults,
}

/// Configuration snapshot recorded with test results.
#[derive(Debug, Serialize, Deserialize)]
pub struct TestConfig {
    /// Target ingress URL.
    pub target: String,
    /// Sequencer RPC URL.
    pub sequencer: String,
    /// Number of wallets used.
    pub wallets: usize,
    /// Target transactions per second.
    pub target_rate: u64,
    /// Test duration in seconds.
    pub duration_secs: u64,
    /// Transaction timeout in seconds.
    pub tx_timeout_secs: u64,
    /// Random seed, if specified.
    pub seed: Option<u64>,
}

/// Throughput metrics from a load test run.
#[derive(Debug, Serialize, Deserialize)]
pub struct ThroughputResults {
    /// Actual send rate in transactions per second.
    pub sent_rate: f64,
    /// Inclusion rate in transactions per second.
    pub included_rate: f64,
    /// Total transactions sent.
    pub total_sent: u64,
    /// Total transactions included on-chain.
    pub total_included: u64,
    /// Total transactions that reverted.
    pub total_reverted: u64,
    /// Total transactions still pending.
    pub total_pending: u64,
    /// Total transactions that timed out.
    pub total_timed_out: u64,
    /// Ratio of included to sent transactions.
    pub success_rate: f64,
    /// Actual test duration in seconds.
    pub actual_duration_secs: f64,
}

/// Error counts from a load test run.
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResults {
    /// Number of send errors.
    pub send_errors: u64,
    /// Number of reverted transactions.
    pub reverted: u64,
    /// Number of timed out transactions.
    pub timed_out: u64,
}

/// Computes aggregated test results from the tracker and configuration.
pub fn calculate_results(tracker: &Arc<TransactionTracker>, config: TestConfig) -> TestResults {
    let actual_duration = tracker.elapsed();
    let total_sent = tracker.total_sent();
    let total_included = tracker.total_included();
    let total_reverted = tracker.total_reverted();
    let total_timed_out = tracker.total_timed_out();
    let send_errors = tracker.total_send_errors();

    let sent_rate = total_sent as f64 / actual_duration.as_secs_f64();
    let included_rate = total_included as f64 / actual_duration.as_secs_f64();
    let success_rate = if total_sent > 0 { total_included as f64 / total_sent as f64 } else { 0.0 };

    TestResults {
        config,
        results: ThroughputResults {
            sent_rate,
            included_rate,
            total_sent,
            total_included,
            total_reverted,
            total_pending: tracker.total_pending(),
            total_timed_out,
            success_rate,
            actual_duration_secs: actual_duration.as_secs_f64(),
        },
        errors: ErrorResults { send_errors, reverted: total_reverted, timed_out: total_timed_out },
    }
}
