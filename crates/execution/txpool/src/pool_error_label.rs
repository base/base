//! Shared pool-error label classification for metrics.

use reth_transaction_pool::error::{PoolError, PoolErrorKind};

use crate::PoolAdmissionError;

/// Maps [`PoolErrorKind`] variants to static metric label strings.
#[derive(Debug)]
pub struct PoolRejectionLabel;

impl PoolRejectionLabel {
    /// Returns a `&'static str` label for the given [`PoolError`].
    pub fn from_error(err: &PoolError) -> &'static str {
        match &err.kind {
            PoolErrorKind::AlreadyImported => "already_imported",
            PoolErrorKind::ReplacementUnderpriced => "replacement_underpriced",
            PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(_) => "fee_cap_below_minimum",
            PoolErrorKind::SpammerExceededCapacity(_) => "spammer_exceeded_capacity",
            PoolErrorKind::DiscardedOnInsert => "discarded_on_insert",
            PoolErrorKind::InvalidTransaction(_) => "invalid_transaction",
            PoolErrorKind::ExistingConflictingTransactionType(_, _) => "conflicting_tx_type",
            PoolErrorKind::Other(_) => match PoolAdmissionError::from_pool_error(err) {
                Some(PoolAdmissionError::LaneConflict) => "lane_conflict",
                Some(PoolAdmissionError::StaleAccountState) => "stale_account_state",
                None => "other",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;

    use super::*;

    #[test]
    fn lane_conflict_has_its_own_label_and_other_errors_keep_theirs() {
        let conflict = PoolAdmissionError::LaneConflict.into_pool_error(TxHash::ZERO);
        assert_eq!(PoolRejectionLabel::from_error(&conflict), "lane_conflict");
        let stale = PoolAdmissionError::StaleAccountState.into_pool_error(TxHash::ZERO);
        assert_eq!(PoolRejectionLabel::from_error(&stale), "stale_account_state");
        assert_eq!(
            PoolRejectionLabel::from_error(&PoolError::other(TxHash::ZERO, "boom")),
            "other"
        );
        assert_eq!(
            PoolRejectionLabel::from_error(&PoolError::new(
                TxHash::ZERO,
                PoolErrorKind::ReplacementUnderpriced
            )),
            "replacement_underpriced"
        );
    }
}
