//! Payload iterators that hold back validity transactions resting under an unchanged predicate.

use std::time::Duration;

use alloy_primitives::TxHash;
use base_execution_txpool::ValidityPredicate;
use revm::state::EvmState;

/// Work an iterator spent holding back resting transactions since it was last asked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestingStats {
    /// Resting transactions parked without being yielded.
    pub parked: u64,
    /// Time spent filtering resting transactions, from the first resting check of each
    /// `next` call of the iterator until it returns, so it includes advancing the iterator
    /// between resting transactions.
    pub duration: Duration,
}

/// Lifecycle callbacks for validity transactions resting in a payload job.
///
/// A transaction rests once the build loop finds one of its predicates unsatisfied. It stays
/// unsatisfied until a later commit in the same block changes the state that predicate reads, so
/// an iterator implementing this trait may hold it back in later flashblocks instead of yielding
/// it to be evaluated again. The default methods hold nothing back.
pub trait RestingPayloadTransactions {
    /// Records that `predicate` was unsatisfied for `transaction_hash` at the current build
    /// position.
    fn rest(&mut self, _transaction_hash: TxHash, _predicate: &ValidityPredicate) {}

    /// Wakes transactions resting on state changed by one committed transaction.
    fn record_committed_state(&mut self, _state: &EvmState) {}

    /// Returns whether a transaction with `predicates` rests under an unchanged predicate.
    fn is_resting(&self, _transaction_hash: TxHash, _predicates: &[ValidityPredicate]) -> bool {
        false
    }

    /// Returns and resets the work spent holding back resting transactions.
    fn take_resting_stats(&mut self) -> RestingStats {
        RestingStats::default()
    }
}
