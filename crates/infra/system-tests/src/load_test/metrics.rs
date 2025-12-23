use super::tracker::TransactionTracker;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct TestResults {
    pub config: TestConfig,
    pub results: ThroughputResults,
    pub errors: ErrorResults,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TestConfig {
    pub target: String,
    pub sequencer: String,
    pub wallets: usize,
    pub target_rate: u64,
    pub duration_secs: u64,
    pub tx_timeout_secs: u64,
    pub seed: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThroughputResults {
    pub sent_rate: f64,
    pub included_rate: f64,
    pub total_sent: u64,
    pub total_included: u64,
    pub total_reverted: u64,
    pub total_pending: u64,
    pub total_timed_out: u64,
    pub success_rate: f64,
    pub actual_duration_secs: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResults {
    pub send_errors: u64,
    pub reverted: u64,
    pub timed_out: u64,
}

pub fn calculate_results(tracker: &Arc<TransactionTracker>, config: TestConfig) -> TestResults {
    let actual_duration = tracker.elapsed();
    let total_sent = tracker.total_sent();
    let total_included = tracker.total_included();
    let total_reverted = tracker.total_reverted();
    let total_timed_out = tracker.total_timed_out();
    let send_errors = tracker.total_send_errors();

    let sent_rate = total_sent as f64 / actual_duration.as_secs_f64();
    let included_rate = total_included as f64 / actual_duration.as_secs_f64();
    let success_rate = if total_sent > 0 {
        total_included as f64 / total_sent as f64
    } else {
        0.0
    };

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
        errors: ErrorResults {
            send_errors,
            reverted: total_reverted,
            timed_out: total_timed_out,
        },
    }
}
