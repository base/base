//! Metrics for the EIP-8130 admission/invalidation guard.
//!
//! All labels are low-cardinality static strings (rejection reasons and
//! invalidation causes); no addresses or hashes are ever used as label values.

base_metrics::define_metrics! {
    txpool.guard,
    struct = GuardMetrics,
    #[describe("EIP-8130 transactions rejected at admission by the dual sender/payer limits")]
    #[label(name = "reason", default = ["sender", "payer", "payer_balance"])]
    admission_rejected: counter,
    #[describe("EIP-8130 transactions invalidated and evicted ahead of the builder")]
    #[label(name = "cause", default = ["state_diff", "expiry", "reconcile"])]
    invalidated: counter,
    #[describe("Expiry buckets fired on canonical updates (one-block lookahead eviction)")]
    expiry_buckets_fired: counter,
    #[describe("Transactions currently tracked by the admission/invalidation guard")]
    tracked: gauge,
    #[describe("EIP-8130 transactions dropped by the builder's stateless manifest pre-check before execution")]
    #[label(name = "cause", default = ["config_slot", "payer_balance", "expiry"])]
    builder_precheck_dropped: counter,
}

impl GuardMetrics {
    /// Static label for the `admission_rejected` `reason`.
    pub const fn rejection_reason(rejection: crate::LimitRejection) -> &'static str {
        match rejection {
            crate::LimitRejection::SenderLimit => "sender",
            crate::LimitRejection::PayerLimit => "payer",
            crate::LimitRejection::PayerBalance => "payer_balance",
        }
    }

    /// Records `count` invalidations attributed to a committed-block state diff
    /// (changed slots, protocol nonces, and payer-balance thresholds).
    pub fn record_state_diff_invalidations(count: usize) {
        if count > 0 {
            Self::invalidated("state_diff").increment(count as u64);
        }
    }

    /// Records `count` invalidations attributed to expiry-bucket firing.
    pub fn record_expiry_invalidations(count: usize) {
        if count > 0 {
            Self::invalidated("expiry").increment(count as u64);
        }
    }

    /// Records `count` stale admission records reclaimed by the per-block reconcile.
    pub fn record_reconcile_releases(count: usize) {
        if count > 0 {
            Self::invalidated("reconcile").increment(count as u64);
        }
    }

    /// Records a builder-side stateless pre-check drop, attributed to the
    /// positively-observed stale [`crate::ManifestStale`] cause.
    pub fn record_builder_precheck_drop(stale: &crate::ManifestStale) {
        Self::builder_precheck_dropped(stale.cause()).increment(1);
    }
}
