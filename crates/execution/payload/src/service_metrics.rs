//! Payload builder service metrics.

use base_common_observability_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Payload builder service metrics
#[derive(Metrics, Clone)]
#[metrics(scope = "payloads")]
pub struct PayloadBuilderServiceMetrics {
    /// Number of active jobs
    pub active_jobs: Gauge,
    /// Total number of initiated jobs
    pub initiated_jobs: Counter,
    /// Total number of failed jobs
    pub failed_jobs: Counter,
    /// Coinbase revenue for best payloads
    pub best_revenue: Gauge,
    /// Current block returned as the best payload
    pub best_block: Gauge,
    /// Coinbase revenue for resolved payloads
    pub resolved_revenue: Gauge,
    /// Current block returned as the resolved payload
    pub resolved_block: Gauge,
    /// Histogram of payload resolve latency in seconds
    pub resolve_duration_seconds: Histogram,
    /// Histogram of new payload job creation latency in seconds
    pub new_job_duration_seconds: Histogram,
}

impl PayloadBuilderServiceMetrics {
    pub fn inc_initiated_jobs(&self) {
        self.initiated_jobs.increment(1);
    }

    pub fn inc_failed_jobs(&self) {
        self.failed_jobs.increment(1);
    }

    pub fn set_active_jobs(&self, value: usize) {
        self.active_jobs.set(value as f64)
    }

    pub fn set_best_revenue(&self, block: u64, value: f64) {
        self.best_block.set(block as f64);
        self.best_revenue.set(value)
    }

    pub fn set_resolved_revenue(&self, block: u64, value: f64) {
        self.resolved_block.set(block as f64);
        self.resolved_revenue.set(value)
    }
}
