//! Payload iterators that hold back validity transactions resting under an unchanged predicate.

use alloy_primitives::TxHash;
use base_execution_payload_builder::{IngestionRestStale, RestingStats};
use base_execution_txpool::{IngestionRest, ValidityConditions, ValidityPredicate};
use revm::state::EvmState;

/// Lifecycle callbacks for validity transactions resting in a payload job.
///
/// A transaction rests once the build loop finds one of its predicates unsatisfied. It stays
/// unsatisfied until a later commit changes the state that predicate reads, so the pool iterators
/// behind an implementation of this trait may hold it back in later flashblocks, or later blocks,
/// instead of yielding it to be evaluated again. The default methods hold nothing back.
pub trait RestingPayloadTransactions {
    /// Records that `predicate` was unsatisfied for `transaction_hash` at the current build
    /// position.
    fn rest(&mut self, _transaction_hash: TxHash, _predicate: &ValidityPredicate) {}

    /// Records that the build loop evaluated a transaction and is not leaving it parked under an
    /// unsatisfied predicate.
    fn release(&mut self, _transaction_hash: TxHash) {}

    /// Wakes transactions resting on state changed by one committed transaction.
    fn record_committed_state(&mut self, _state: &EvmState) {}

    /// Returns whether a transaction with `predicates` rests under an unchanged predicate.
    fn is_resting(&self, _transaction_hash: TxHash, _predicates: &ValidityConditions) -> bool {
        false
    }

    /// Returns whether the pool's admission-time evaluation `rest` of a candidate with
    /// `conditions` still holds; see
    /// [`RestingPredicates::ingestion_rest_applies`](base_execution_payload_builder::RestingPredicates::ingestion_rest_applies).
    /// The default tracks nothing, so no result applies.
    fn ingestion_rest_applies(
        &self,
        _rest: &IngestionRest,
        _conditions: &ValidityConditions,
    ) -> Result<(), IngestionRestStale> {
        Err(IngestionRestStale::Untracked)
    }

    /// Returns the transactions the pool iterators adopted on their admission-time evaluation
    /// since the last call and that still rest, so the build loop can report their deferral.
    fn take_adopted(&mut self) -> Vec<TxHash> {
        Vec::new()
    }

    /// Returns and resets the work spent holding back resting transactions.
    fn take_resting_stats(&mut self) -> RestingStats {
        RestingStats::default()
    }
}
