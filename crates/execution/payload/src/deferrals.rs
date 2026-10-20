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
}
