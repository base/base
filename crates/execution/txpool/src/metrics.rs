//! Metrics for EIP-8130 admission, invalidation, builder prechecks, and
//! transaction validation.
//!
//! All labels are low-cardinality static categories; addresses and transaction
//! hashes are never used as label values.

base_metrics::define_metrics! {
    txpool.guard,
    struct = GuardMetrics,
    #[describe("EIP-8130 transactions rejected at admission by signature/payment limits")]
    #[label(name = "reason", default = ["sender", "payer", "payment", "payer_balance"])]
    admission_rejected: counter,
    #[describe("EIP-8130 transactions invalidated and evicted ahead of the builder")]
    #[label(
        name = "cause",
        default = [
            "state_diff",
            "balance_update",
            "expiry",
            "block_expiry",
            "reorg",
            "feed_gap",
            "reconcile"
        ]
    )]
    invalidated: counter,
    #[describe("Occupied expiry buckets fired on canonical state updates")]
    expiry_buckets_fired: counter,
    #[describe("Transactions currently tracked by the admission/invalidation guard")]
    tracked: gauge,
    #[describe("EIP-8130 drop events from the builder's stateless manifest precheck")]
    #[label(name = "cause", default = ["config_slot", "payer_balance", "expiry"])]
    builder_precheck_dropped: counter,
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

    /// Records validity-predicate transactions evicted once the chain advanced
    /// past their last valid block.
    pub fn record_block_expiry_invalidations(count: usize) {
        if count > 0 {
            Self::invalidated("block_expiry").increment(count as u64);
        }
    }

    /// Records bulk invalidations that flush every guarded transaction, labeled
    /// by cause so a common reorg is distinguishable from a rare feed gap.
    pub fn record_bulk_invalidations(count: usize, cause: crate::InvalidationCause) {
        if count > 0 {
            Self::invalidated(cause.as_label()).increment(count as u64);
        }
    }

    /// Records stale admission records reclaimed by canonical reconciliation.
    pub fn record_reconcile_releases(count: usize) {
        if count > 0 {
            Self::invalidated("reconcile").increment(count as u64);
        }
    }

    /// Records a builder precheck drop by its positively observed stale cause.
    pub fn record_builder_precheck_drop(stale: &crate::ManifestStale) {
        Self::builder_precheck_dropped(stale.cause()).increment(1);
    }
}

base_metrics::define_metrics! {
    txpool.validator,
    struct = ValidatorMetrics,
    #[describe("End-to-end mempool validation wall time by transaction kind")]
    #[label(name = "kind", default = ["eip8130", "standard"])]
    validate_seconds: histogram,
    #[describe("EIP-8130 authorization wall time by sender authenticator type")]
    #[label(name = "sig_type", default = ["k1", "p256", "passkey", "delegate", "delegate-k1", "delegate-p256", "delegate-passkey", "other"])]
    auth_seconds: histogram,
    #[describe("EIP-8130 lock-classification account-state resolutions by read source")]
    #[label(name = "source", default = ["cache", "prefetch", "sload"])]
    classification_state_reads: counter,
}
