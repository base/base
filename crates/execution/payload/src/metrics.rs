//! Metrics for native payload resource metering by opcode.

use crate::resource_metering::{ResourceThrottlingLimitExceeded, ResourceThrottlingLimitScope};

base_metrics::define_metrics! {
    base_payload.resource_metering,
    struct = ResourceMeteringMetrics,
    #[describe("Transactions whose resource-metering usage could not be calculated")]
    calculation_failed: counter,
    #[describe("Transactions that would be throttled by a resource-metering budget")]
    #[label(dimension)]
    #[label(scope)]
    limit_exceeded_total: counter,
    #[describe("Transactions throttled by a resource-metering budget")]
    #[label(dimension)]
    #[label(scope)]
    throttled_total: counter,
}

impl ResourceMeteringMetrics {
    /// Records a budget observation, and a throttle when the builder enforces the schedule.
    pub fn record_limit(error: &ResourceThrottlingLimitExceeded, enforced: bool) {
        let scope = error.scope.as_metric_label().to_string();
        Self::limit_exceeded_total(error.dimension.clone(), scope.clone()).increment(1);
        if enforced {
            Self::throttled_total(error.dimension.clone(), scope).increment(1);
        }
    }
}

impl ResourceThrottlingLimitScope {
    /// Returns the low-cardinality metric label for this scope.
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::Transaction => "transaction",
            Self::Block => "block",
        }
    }
}
