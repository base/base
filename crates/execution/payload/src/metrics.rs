//! Metrics for native payload resource metering by opcode.

use crate::resource_metering::{ResourceMeteringLimitExceeded, ResourceMeteringLimitScope};

base_metrics::define_metrics! {
    base_payload.resource_metering,
    struct = ResourceMeteringMetrics,
    #[describe("Transactions whose resource-metering usage could not be calculated")]
    calculation_failed: counter,
    #[describe("Transactions that would be rejected by a resource-metering budget")]
    #[label(dimension)]
    #[label(scope)]
    limit_exceeded_total: counter,
    #[describe("Transactions skipped by a resource-metering budget")]
    #[label(dimension)]
    #[label(scope)]
    rejected_total: counter,
    #[describe("Resource-metering schedule replacements")]
    schedule_updates_total: counter,
    #[describe("Active resource-metering schedule revision")]
    schedule_revision: gauge,
}

impl ResourceMeteringMetrics {
    /// Records a budget observation, and a rejection when the builder enforces the schedule.
    pub fn record_limit(error: &ResourceMeteringLimitExceeded, enforced: bool) {
        let scope = error.scope.as_metric_label().to_string();
        Self::limit_exceeded_total(error.dimension.clone(), scope.clone()).increment(1);
        if enforced {
            Self::rejected_total(error.dimension.clone(), scope).increment(1);
        }
    }

    /// Records the active schedule revision after a replacement or startup load.
    pub fn record_schedule_revision(revision: u64) {
        Self::schedule_revision().set(revision as f64);
    }
}

impl ResourceMeteringLimitScope {
    /// Returns the low-cardinality metric label for this scope.
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::Transaction => "transaction",
            Self::Block => "block",
        }
    }
}
