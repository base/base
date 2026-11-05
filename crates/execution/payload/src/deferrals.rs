use alloy_primitives::{TxHash, map::HashSet};

/// The deferrals journaled as `BUILDER_DEFERRED` for the block being built.
///
/// A validity-gated transaction that stays blocked is parked again after every promotion, in
/// every following block until its predicate is satisfied. A build reports a deferral only when
/// it tells the journal something new: the first time a transaction is deferred for a reason in
/// that build.
#[derive(Debug, Default)]
pub struct BlockDeferrals {
    seen: HashSet<(TxHash, &'static str)>,
}

impl BlockDeferrals {
    /// Records that `tx_hash` was deferred for `reason`, returning `true` when it has not
    /// already been recorded for that reason in this build.
    pub fn record(&mut self, tx_hash: TxHash, reason: &'static str) -> bool {
        self.seen.insert((tx_hash, reason))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    const NOT_SATISFIED: &str = "validity_predicate_not_satisfied";
    const BUDGET_EXHAUSTED: &str = "predicate_eval_budget_exhausted";

    #[test]
    fn repeated_deferral_for_the_same_reason_is_recorded_once() {
        let mut deferrals = BlockDeferrals::default();
        let tx = B256::repeat_byte(1);

        assert!(deferrals.record(tx, NOT_SATISFIED));
        assert!(!deferrals.record(tx, NOT_SATISFIED));
        assert!(deferrals.record(B256::repeat_byte(2), NOT_SATISFIED));
    }

    #[test]
    fn each_reason_is_recorded_once() {
        let mut deferrals = BlockDeferrals::default();
        let tx = B256::repeat_byte(1);

        assert!(deferrals.record(tx, NOT_SATISFIED));
        assert!(deferrals.record(tx, BUDGET_EXHAUSTED));
        assert!(!deferrals.record(tx, BUDGET_EXHAUSTED));
        assert!(!deferrals.record(tx, NOT_SATISFIED));
    }

    #[test]
    fn default_deferrals_do_not_carry_into_the_next_build() {
        let tx = B256::repeat_byte(1);
        assert!(BlockDeferrals::default().record(tx, NOT_SATISFIED));
        assert!(BlockDeferrals::default().record(tx, NOT_SATISFIED));
    }
}
