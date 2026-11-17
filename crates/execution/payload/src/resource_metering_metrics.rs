//! Metrics for payload resource metering and the native rejection cache.

use crate::{
    ContractSlotLimitExceeded,
    resource_metering::{ResourceThrottlingLimitExceeded, ResourceThrottlingLimitScope},
};

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
    #[describe("Executed transactions that a dry-run per-contract changed-slot cap would limit")]
    #[label(scope)]
    #[label(depth)]
    contract_slot_limit_exceeded_total: counter,
}

base_metrics::define_metrics! {
    base_payload.rejection_cache,
    struct = RejectionCacheMetrics,
    #[describe("Transactions inserted into the native payload rejection cache")]
    insertions: counter,
    #[describe("Transactions skipped because they were already in the native payload rejection cache")]
    hits: counter,
    #[describe("Number of entries in the native payload rejection cache")]
    size: gauge,
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

    /// Records a dry-run per-contract changed-slot overrun.
    ///
    /// The owner address stays out of the labels; the depth class is bounded
    /// by [`crate::MAX_TRIE_DEPTH`], and `flat` marks the flat cap.
    pub fn record_contract_slot_limit(error: &ContractSlotLimitExceeded) {
        let depth = error.depth.map_or_else(|| "flat".to_string(), |depth| depth.to_string());
        Self::contract_slot_limit_exceeded_total(error.scope.as_metric_label(), depth).increment(1);
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
