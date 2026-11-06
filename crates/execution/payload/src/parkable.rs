//! Payload transaction adapters with lane-aware parking support.

use std::sync::Arc;

use alloy_primitives::{Address, TxHash};
use base_execution_txpool::{BasePooledTx, ParkableBestTransactions, ValidityPredicate};
pub use reth_payload_util::NoopPayloadTransactions;
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::{
    PoolTransaction, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};
use revm::state::EvmState;

/// Indicates that the payload builder excluded a transaction from the current candidate iterator.
#[derive(Debug, thiserror::Error)]
#[error("transaction invalidated during payload construction")]
pub struct PayloadTransactionInvalidated;

impl PoolTransactionError for PayloadTransactionInvalidated {
    fn is_bad_transaction(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Parking lifecycle callbacks used in addition to payload iteration.
///
/// A transaction returned by [`PayloadTransactions::next`] becomes current until the caller parks,
/// commits, or invalidates it. Current-transaction callbacks use the exact validated pool
/// transaction retained by the adapter rather than reconstructing its identity from a hash.
///
/// Candidates are yielded as the pool's shared [`ValidPoolTransaction`] handle, so callers clone
/// the consensus transaction only for candidates they actually include.
pub trait ParkablePayloadTransactions:
    PayloadTransactions<Transaction = Arc<ValidPoolTransaction<Self::Pooled>>>
{
    /// The pool transaction type wrapped by each yielded candidate.
    type Pooled: PoolTransaction;

    /// Parks and clears the current transaction, if any.
    fn park_current(&mut self);

    /// Commits and clears the current transaction, if any.
    fn mark_current_committed(&mut self);

    /// Promotes a predicate-parked transaction back into priority competition.
    fn promote(&mut self, transaction_hash: TxHash) -> bool;

    /// Excludes a predicate-parked transaction for the remainder of this iterator.
    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool;

    /// Whether unsatisfied transactions are parked in, and woken from, the pool's dormancy index
    /// through [`Self::park_current_unsatisfied`] and [`Self::wake_by_state`] instead of an index
    /// the build keeps. The default keeps none.
    fn parks_in_pool_index(&self) -> bool {
        false
    }

    /// Parks the current transaction, whose validity predicate `predicate` failed, in the
    /// iterator and the pool's dormancy index. The default only parks it.
    fn park_current_unsatisfied(&mut self, _predicate: ValidityPredicate) {
        self.park_current();
    }

    /// Wakes the transactions in the pool's dormancy index that a committed transaction's `state`
    /// may satisfy. The default wakes nothing.
    fn wake_by_state(&mut self, _state: &EvmState) {}
}

impl<T> ParkablePayloadTransactions for NoopPayloadTransactions<Arc<ValidPoolTransaction<T>>>
where
    T: PoolTransaction,
{
    type Pooled = T;

    fn park_current(&mut self) {}

    fn mark_current_committed(&mut self) {}

    fn promote(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }

    fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }
}

/// Converts a parkable best iterator into the payload-transaction interface used by the builder.
pub struct ParkableBestPayloadTransactions<T>
where
    T: BasePooledTx,
{
    inner: Box<dyn ParkableBestTransactions<T>>,
    current: Option<Arc<ValidPoolTransaction<T>>>,
}

impl<T> std::fmt::Debug for ParkableBestPayloadTransactions<T>
where
    T: BasePooledTx,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkableBestPayloadTransactions")
            .field("has_current", &self.current.is_some())
            .finish_non_exhaustive()
    }
}

impl<T> ParkableBestPayloadTransactions<T>
where
    T: BasePooledTx,
{
    /// Creates a payload adapter over a parkable best iterator.
    pub fn new(inner: Box<dyn ParkableBestTransactions<T>>) -> Self {
        Self { inner, current: None }
    }
}

impl<T> PayloadTransactions for ParkableBestPayloadTransactions<T>
where
    T: BasePooledTx,
{
    type Transaction = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        debug_assert!(
            self.current.is_none(),
            "previous transaction must be lifecycle-managed before next"
        );
        let transaction = self.inner.next()?;
        self.current = Some(Arc::clone(&transaction));
        Some(transaction)
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        let Some(transaction) = self.current.as_ref() else {
            return;
        };
        let matches_current =
            transaction.sender() == sender && transaction.transaction.nonce() == nonce;
        debug_assert!(matches_current, "mark_invalid must identify the current transaction");
        if !matches_current {
            return;
        }
        let transaction = self.current.take().expect("current transaction was checked above");
        self.inner.mark_invalid(
            &transaction,
            InvalidPoolTransactionError::other(PayloadTransactionInvalidated),
        );
    }
}

impl<T> ParkablePayloadTransactions for ParkableBestPayloadTransactions<T>
where
    T: BasePooledTx,
{
    type Pooled = T;

    fn park_current(&mut self) {
        if let Some(transaction) = self.current.take() {
            self.inner.park(&transaction);
        }
    }

    fn mark_current_committed(&mut self) {
        if let Some(transaction) = self.current.take() {
            self.inner.mark_committed(&transaction);
        }
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.discard_parked(
            transaction_hash,
            InvalidPoolTransactionError::other(PayloadTransactionInvalidated),
        )
    }

    fn parks_in_pool_index(&self) -> bool {
        self.inner.parks_in_pool_index()
    }

    fn park_current_unsatisfied(&mut self, predicate: ValidityPredicate) {
        if let Some(transaction) = self.current.take() {
            self.inner.park_unsatisfied(&transaction, predicate);
        }
    }

    fn wake_by_state(&mut self, state: &EvmState) {
        self.inner.wake_by_state(state);
    }
}
