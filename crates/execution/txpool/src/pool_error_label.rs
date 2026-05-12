//! Shared pool-error label classification for metrics.

use reth_transaction_pool::error::{PoolError, PoolErrorKind};

/// Maps [`PoolErrorKind`] variants to static metric label strings.
#[derive(Debug)]
pub struct PoolRejectionLabel;

impl PoolRejectionLabel {
    /// Returns a `&'static str` label for the given [`PoolError`].
    pub const fn from_error(err: &PoolError) -> &'static str {
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
