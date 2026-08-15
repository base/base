//! Boundless proving metrics.

use std::time::Instant;

base_metrics::define_metrics! {
    base_registrar,
    struct = BoundlessMetrics,

    #[describe("Total number of Boundless proof requests submitted onchain")]
    boundless_requests_submitted_total: counter,

    #[describe("Total number of Boundless recovery scan outcomes")]
    #[label(name = "outcome", default = ["locked", "fulfilled", "expired", "unknown", "request_not_locked", "stale", "failed", "blocked", "query_error", "random_fallback"])]
    boundless_recovery_total: counter,

    #[describe("Total number of Boundless fulfillment poll outcomes")]
    #[label(name = "outcome", default = ["succeeded", "failed", "request_not_locked_retry"])]
    boundless_fulfillment_total: counter,

    #[describe("Total number of Boundless receipt fetch outcomes")]
    #[label(name = "outcome", default = ["succeeded", "failed", "retry", "stale", "encode_failed"])]
    boundless_receipt_fetch_total: counter,

    #[describe("Duration of Boundless proof generation in milliseconds")]
    #[label(name = "outcome", default = ["succeeded", "failed", "cancelled"])]
    boundless_proof_duration_ms: histogram,
}

impl BoundlessMetrics {
    /// Boundless recovery found a locked request.
    pub const RECOVERY_OUTCOME_LOCKED: &'static str = "locked";
    /// Boundless recovery found a fulfilled request.
    pub const RECOVERY_OUTCOME_FULFILLED: &'static str = "fulfilled";
    /// Boundless recovery found an expired request.
    pub const RECOVERY_OUTCOME_EXPIRED: &'static str = "expired";
    /// Boundless recovery found an unused request slot.
    pub const RECOVERY_OUTCOME_UNKNOWN: &'static str = "unknown";
    /// Boundless recovery hit the `RequestIsNotLocked` race path.
    pub const RECOVERY_OUTCOME_REQUEST_NOT_LOCKED: &'static str = "request_not_locked";
    /// Boundless recovery skipped a stale fulfilled proof.
    pub const RECOVERY_OUTCOME_STALE: &'static str = "stale";
    /// Boundless recovery failed while trying to resume a request.
    pub const RECOVERY_OUTCOME_FAILED: &'static str = "failed";
    /// Boundless recovery skipped a slot because recovery was blocked for the signer.
    pub const RECOVERY_OUTCOME_BLOCKED: &'static str = "blocked";
    /// Boundless recovery failed while querying request status.
    pub const RECOVERY_OUTCOME_QUERY_ERROR: &'static str = "query_error";
    /// Boundless recovery exhausted deterministic slots and fell back to a random request ID.
    pub const RECOVERY_OUTCOME_RANDOM_FALLBACK: &'static str = "random_fallback";

    /// Boundless fulfillment polling succeeded.
    pub const FULFILLMENT_OUTCOME_SUCCEEDED: &'static str = "succeeded";
    /// Boundless fulfillment polling failed.
    pub const FULFILLMENT_OUTCOME_FAILED: &'static str = "failed";
    /// Boundless fulfillment polling retried the `RequestIsNotLocked` race path.
    pub const FULFILLMENT_OUTCOME_REQUEST_NOT_LOCKED_RETRY: &'static str =
        "request_not_locked_retry";

    /// Boundless receipt fetch succeeded.
    pub const RECEIPT_FETCH_OUTCOME_SUCCEEDED: &'static str = "succeeded";
    /// Boundless receipt fetch failed permanently.
    pub const RECEIPT_FETCH_OUTCOME_FAILED: &'static str = "failed";
    /// Boundless receipt fetch hit a retryable failure.
    pub const RECEIPT_FETCH_OUTCOME_RETRY: &'static str = "retry";
    /// Boundless receipt fetch skipped a stale fulfilled proof.
    pub const RECEIPT_FETCH_OUTCOME_STALE: &'static str = "stale";
    /// Boundless receipt fetch succeeded but seal encoding failed.
    pub const RECEIPT_FETCH_OUTCOME_ENCODE_FAILED: &'static str = "encode_failed";

    /// Boundless proof generation succeeded.
    pub const PROOF_OUTCOME_SUCCEEDED: &'static str = "succeeded";
    /// Boundless proof generation failed.
    pub const PROOF_OUTCOME_FAILED: &'static str = "failed";
    /// Boundless proof generation was cancelled.
    pub const PROOF_OUTCOME_CANCELLED: &'static str = "cancelled";

    /// Records a full Boundless proof-generation duration.
    pub fn record_proof_duration(started_at: Instant, outcome: &'static str) {
        Self::boundless_proof_duration_ms(outcome)
            .record(started_at.elapsed().as_secs_f64() * 1000.0);
    }
}
