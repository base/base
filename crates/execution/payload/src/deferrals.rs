//! Per-block dedupe of repeated builder decision events.
use alloy_primitives::{TxHash, map::B256Map};

/// The defer reason last journaled as `BUILDER_DEFERRED` for each transaction in the block being
/// built.
///
/// A validity-gated transaction that stays blocked is parked again after every promotion, and by
/// the flashblocks builder on every flashblock, so builders report a deferral only when it tells
/// the journal something new. Create one per block build; it is never pruned during the build.
#[derive(Debug, Default)]
pub struct BlockDeferrals {
    reasons: B256Map<&'static str>,
}

impl BlockDeferrals {
    /// Records that `tx_hash` was deferred for `reason`, returning `true` when this is its first
    /// deferral in the block or the reason differs from the last one recorded.
    pub fn record(&mut self, tx_hash: TxHash, reason: &'static str) -> bool {
        self.reasons.insert(tx_hash, reason) != Some(reason)
    }
}

/// The rejection reason last journaled as `BUILDER_REJECTED` for each transaction in the block
/// being built.
///
/// Mirrors [`BlockDeferrals`]: a transaction that overflows a flashblock's cumulative gas, DA, or
/// size budget is rejected again on every later flashblock, so builders report a rejection only
/// when it tells the journal something new. Create one per block build and drop it when the block
/// is sealed; a rejection in the next block is a new event because the journal starts empty. It is
/// never pruned during the build, so it holds at most one entry per transaction rejected at least
/// once in the block.
#[derive(Debug, Default)]
pub struct BlockRejections {
    reasons: B256Map<&'static str>,
}

impl BlockRejections {
    /// Records that `tx_hash` was rejected for `reason`, returning `true` when this is its first
    /// rejection in the block or the reason differs from the last one recorded.
    pub fn record(&mut self, tx_hash: TxHash, reason: &'static str) -> bool {
        self.reasons.insert(tx_hash, reason) != Some(reason)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    #[test]
    fn repeated_deferral_for_the_same_reason_is_recorded_once() {
        let mut deferrals = BlockDeferrals::default();
        let tx = B256::repeat_byte(1);

        assert!(deferrals.record(tx, "validity_predicate_not_satisfied"));
        assert!(!deferrals.record(tx, "validity_predicate_not_satisfied"));
        assert!(deferrals.record(B256::repeat_byte(2), "validity_predicate_not_satisfied"));
    }

    #[test]
    fn a_changed_reason_is_recorded_again() {
        let mut deferrals = BlockDeferrals::default();
        let tx = B256::repeat_byte(1);

        assert!(deferrals.record(tx, "validity_predicate_not_satisfied"));
        assert!(deferrals.record(tx, "predicate_eval_budget_exhausted"));
        assert!(!deferrals.record(tx, "predicate_eval_budget_exhausted"));
        assert!(deferrals.record(tx, "validity_predicate_not_satisfied"));
    }

    #[test]
    fn repeated_rejection_for_the_same_reason_is_recorded_once() {
        let mut rejections = BlockRejections::default();
        let tx = B256::repeat_byte(1);

        assert!(rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(!rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(rejections.record(B256::repeat_byte(2), "transaction_gas_limit_exceeded"));
    }

    #[test]
    fn a_changed_rejection_reason_is_recorded_again() {
        let mut rejections = BlockRejections::default();
        let tx = B256::repeat_byte(1);

        assert!(rejections.record(tx, "transaction_gas_limit_exceeded"));
        assert!(rejections.record(tx, "block_da_size_exceeded"));
        assert!(!rejections.record(tx, "block_da_size_exceeded"));
        assert!(rejections.record(tx, "transaction_gas_limit_exceeded"));
    }

    #[test]
    fn a_fresh_journal_records_the_reason_again_for_the_next_block() {
        let tx = B256::repeat_byte(1);

        let mut block_n = BlockRejections::default();
        assert!(block_n.record(tx, "transaction_gas_limit_exceeded"));
        assert!(!block_n.record(tx, "transaction_gas_limit_exceeded"));

        // The next block build starts an empty journal, so the same rejection is reported again.
        let mut block_n_plus_1 = BlockRejections::default();
        assert!(block_n_plus_1.record(tx, "transaction_gas_limit_exceeded"));
    }
}
