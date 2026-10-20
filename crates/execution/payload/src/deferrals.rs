use std::sync::Arc;

use alloy_primitives::{TxHash, map::HashMap};
use parking_lot::Mutex;

/// Number of blocks a deferral stays remembered after the last block that deferred the
/// transaction for the same reason.
///
/// A parked validity transaction is normally deferred again in every block until it is included
/// or leaves the pool. A block that stops building before reaching it should not restart its
/// streak, so a short gap is tolerated.
const DEFERRAL_RETAINED_BLOCKS: u64 = 4;

/// Last block in which each transaction was deferred for each reason.
type DeferralBlocks = HashMap<(TxHash, &'static str), u64>;

/// The deferrals journaled as `BUILDER_DEFERRED` for the block being built.
///
/// A validity-gated transaction that stays blocked is parked again after every promotion, by
/// the flashblocks builder on every flashblock, and again in every following block until its
/// predicate is satisfied. Builders report a deferral only when it tells the journal something
/// new: the first time a transaction is deferred for a reason.
///
/// [`BlockDeferrals::default`] remembers deferrals for one block build only. A block started from
/// a [`DeferralJournal`] also remembers deferrals from recent blocks, and hands its own back to
/// the journal when dropped.
#[derive(Debug, Default)]
pub struct BlockDeferrals {
    block_number: u64,
    seen: DeferralBlocks,
    journal: Option<Arc<Mutex<DeferralBlocks>>>,
}

impl BlockDeferrals {
    /// Records that `tx_hash` was deferred for `reason`, returning `true` when it has not
    /// already been recorded for that reason in this block or a recent one.
    pub fn record(&mut self, tx_hash: TxHash, reason: &'static str) -> bool {
        self.seen.insert((tx_hash, reason), self.block_number).is_none()
    }
}

impl Drop for BlockDeferrals {
    fn drop(&mut self) {
        if let Some(journal) = self.journal.take() {
            *journal.lock() = std::mem::take(&mut self.seen);
        }
    }
}

/// Deferrals carried from one block build to the next, so a transaction that stays parked for
/// several blocks is journaled as deferred once rather than once per block.
///
/// Share one journal per builder. If two builds overlap, the one that finishes last decides what
/// the next block remembers, which can only cause repeated events, never missing ones.
#[derive(Debug, Clone, Default)]
pub struct DeferralJournal {
    recent: Arc<Mutex<DeferralBlocks>>,
}

impl DeferralJournal {
    /// Starts tracking deferrals for `block_number`, forgetting any not repeated within the last
    /// [`DEFERRAL_RETAINED_BLOCKS`] blocks.
    pub fn begin_block(&self, block_number: u64) -> BlockDeferrals {
        let mut seen = std::mem::take(&mut *self.recent.lock());
        let oldest = block_number.saturating_sub(DEFERRAL_RETAINED_BLOCKS);
        seen.retain(|_, last| *last >= oldest);
        BlockDeferrals { block_number, seen, journal: Some(Arc::clone(&self.recent)) }
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
    fn default_deferrals_do_not_carry_into_the_next_block() {
        let tx = B256::repeat_byte(1);
        assert!(BlockDeferrals::default().record(tx, NOT_SATISFIED));
        assert!(BlockDeferrals::default().record(tx, NOT_SATISFIED));
    }

    #[test]
    fn deferral_in_consecutive_blocks_is_recorded_once() {
        let journal = DeferralJournal::default();
        let tx = B256::repeat_byte(1);

        for block in 100..110 {
            let mut deferrals = journal.begin_block(block);
            assert_eq!(deferrals.record(tx, NOT_SATISFIED), block == 100, "block {block}");
        }
    }

    #[test]
    fn deferral_is_recorded_again_after_a_long_gap() {
        let journal = DeferralJournal::default();
        let tx = B256::repeat_byte(1);

        assert!(journal.begin_block(100).record(tx, NOT_SATISFIED));
        assert!(!journal.begin_block(100 + DEFERRAL_RETAINED_BLOCKS).record(tx, NOT_SATISFIED));

        let last = 100 + DEFERRAL_RETAINED_BLOCKS;
        assert!(journal.begin_block(last + DEFERRAL_RETAINED_BLOCKS + 1).record(tx, NOT_SATISFIED));
    }

    #[test]
    fn transactions_not_deferred_in_recent_blocks_are_forgotten() {
        let journal = DeferralJournal::default();
        let parked = B256::repeat_byte(1);
        let included = B256::repeat_byte(2);

        {
            let mut deferrals = journal.begin_block(100);
            deferrals.record(parked, NOT_SATISFIED);
            deferrals.record(included, NOT_SATISFIED);
        }
        for block in 101..=100 + DEFERRAL_RETAINED_BLOCKS + 1 {
            journal.begin_block(block).record(parked, NOT_SATISFIED);
        }

        assert_eq!(journal.recent.lock().len(), 1);
    }
}
