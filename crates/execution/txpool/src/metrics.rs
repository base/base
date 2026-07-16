//! Metrics for the EIP-8130 admission and invalidation guard.

base_metrics::define_metrics! {
    txpool.guard,
    struct = GuardMetrics,
    #[describe("EIP-8130 transactions rejected at admission by signature/payment limits")]
    #[label(name = "reason", default = ["sender", "payer", "payment", "payer_balance"])]
    admission_rejected: counter,
    #[describe("EIP-8130 transactions invalidated and evicted ahead of the builder")]
    #[label(name = "cause", default = ["state_diff", "balance_update", "expiry", "reconcile"])]
    invalidated: counter,
    #[describe("Expiry buckets fired on canonical updates (one-block lookahead eviction)")]
    expiry_buckets_fired: counter,
    #[describe("Transactions currently tracked by the admission/invalidation guard")]
    tracked: gauge,
}

impl GuardMetrics {
    /// Static label for the `admission_rejected` reason.
    pub const fn rejection_reason(rejection: crate::LimitRejection) -> &'static str {
        match rejection {
            crate::LimitRejection::SenderLimit => "sender",
            crate::LimitRejection::PayerLimit => "payer",
            crate::LimitRejection::PaymentLimit => "payment",
            crate::LimitRejection::PayerBalance => "payer_balance",
        }
    }

    /// Records invalidations attributed to a committed-block state diff.
    pub fn record_state_diff_invalidations(count: usize) {
        if count > 0 {
            Self::invalidated("state_diff").increment(count as u64);
        }
    }

    /// Records invalidations attributed to reth's balance-update path.
    pub fn record_balance_update_invalidations(count: usize) {
        if count > 0 {
            Self::invalidated("balance_update").increment(count as u64);
        }
    }

    /// Records invalidations attributed to expiry-bucket firing.
    pub fn record_expiry_invalidations(count: usize) {
        if count > 0 {
            Self::invalidated("expiry").increment(count as u64);
        }
    }

    /// Records stale admission records reclaimed by canonical reconciliation.
    pub fn record_reconcile_releases(count: usize) {
        if count > 0 {
            Self::invalidated("reconcile").increment(count as u64);
        }
    }
}
