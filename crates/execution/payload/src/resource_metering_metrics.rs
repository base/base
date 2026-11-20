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
        let depth = error
            .depth
            .and_then(|depth| Self::DEPTH_LABELS.get(usize::from(depth)).copied())
            .unwrap_or("flat");
        Self::contract_slot_limit_exceeded_total(error.scope.as_metric_label(), depth).increment(1);
    }

    /// Static depth-class labels, indexed by depth, so recording does not allocate.
    const DEPTH_LABELS: [&'static str; crate::MAX_TRIE_DEPTH as usize + 1] = [
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
        "17", "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30", "31",
        "32", "33", "34", "35", "36", "37", "38", "39", "40", "41", "42", "43", "44", "45", "46",
        "47", "48", "49", "50", "51", "52", "53", "54", "55", "56", "57", "58", "59", "60", "61",
        "62", "63", "64",
    ];
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
