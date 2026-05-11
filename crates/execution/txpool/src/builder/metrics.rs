//! Metrics for the builder RPC handler.

use reth_transaction_pool::error::{PoolError, PoolErrorKind};

base_metrics::define_metrics! {
    txpool.builder_rpc
    #[describe("Transactions successfully inserted into the pool")]
    txs_inserted: counter,
    #[describe("Transactions that failed to decode")]
    decode_errors: counter,
    #[describe("Transactions rejected by the pool")]
    #[label(name = "reason", default = [
        "already_imported",
        "replacement_underpriced",
        "fee_cap_below_minimum",
        "spammer_exceeded_capacity",
        "discarded_on_insert",
        "invalid_transaction",
        "conflicting_tx_type",
        "other",
    ])]
    txs_rejected: counter,
    #[describe("Time to insert a transaction in the local txpool")]
    insert_duration: histogram,
}

impl Metrics {
    /// Maps a [`PoolError`] to a static label for the `txs_rejected` metric.
    pub const fn rejection_label(err: &PoolError) -> &'static str {
        match &err.kind {
            PoolErrorKind::AlreadyImported => "already_imported",
            PoolErrorKind::ReplacementUnderpriced => "replacement_underpriced",
            PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(_) => "fee_cap_below_minimum",
            PoolErrorKind::SpammerExceededCapacity(_) => "spammer_exceeded_capacity",
            PoolErrorKind::DiscardedOnInsert => "discarded_on_insert",
            PoolErrorKind::InvalidTransaction(_) => "invalid_transaction",
            PoolErrorKind::ExistingConflictingTransactionType(_, _) => "conflicting_tx_type",
            PoolErrorKind::Other(_) => "other",
        }
    }
}
