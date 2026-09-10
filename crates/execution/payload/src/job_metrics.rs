//! Metrics for the payload builder impl

use base_common_observability_metrics::{Metrics, metrics::Counter};

/// Payload builder metrics
#[derive(Metrics)]
#[metrics(scope = "payloads")]
pub struct PayloadBuilderMetrics {
    /// Total number of times an empty payload was returned because a built one was not ready.
    pub requested_empty_payload: Counter,
    /// Total number of initiated payload build attempts.
    pub initiated_payload_builds: Counter,
    /// Total number of failed payload build attempts.
    pub failed_payload_builds: Counter,
}

impl PayloadBuilderMetrics {
    pub fn inc_requested_empty_payload(&self) {
        self.requested_empty_payload.increment(1);
    }

    pub fn inc_initiated_payload_builds(&self) {
        self.initiated_payload_builds.increment(1);
    }

    pub fn inc_failed_payload_builds(&self) {
        self.failed_payload_builds.increment(1);
    }
}
