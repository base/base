//! Payload transaction adapters with lane-aware parking support.

use std::sync::Arc;

use alloy_primitives::{Address, TxHash};
use base_execution_txpool::{BasePooledTx, ParkableBestTransactions};
pub use reth_payload_util::NoopPayloadTransactions;
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};

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
pub trait ParkablePayloadTransactions: PayloadTransactions
where
    Self::Transaction: PoolTransaction,
{
    /// Parks and clears the current transaction.
    ///
    /// Returns `false` when this iterator does not support parking or has no current transaction.
    fn park_current(&mut self) -> bool;

    /// Commits and clears the current transaction, if any.
    fn mark_current_committed(&mut self);

    /// Promotes a predicate-parked transaction back into priority competition.
    fn promote(&mut self, transaction_hash: TxHash) -> bool;

    /// Excludes a predicate-parked transaction for the remainder of this iterator.
    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool;
}

impl<T, I> ParkablePayloadTransactions for reth_payload_util::BestPayloadTransactions<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    fn park_current(&mut self) -> bool {
        false
    }

    fn mark_current_committed(&mut self) {}

    fn promote(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }

    fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }
}

impl<T> ParkablePayloadTransactions for reth_payload_util::NoopPayloadTransactions<T>
where
    T: PoolTransaction,
{
    fn park_current(&mut self) -> bool {
        false
    }

    fn mark_current_committed(&mut self) {}

    fn promote(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }

    fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }
}

/// Adds no-op parking lifecycle methods to a payload iterator that cannot park transactions.
///
/// The payload builder skips transactions with unsatisfied predicates when this adapter reports
/// that parking is unavailable.
#[derive(Debug, Clone)]
pub struct NonParkablePayloadTransactions<I> {
    inner: I,
}

impl<I> NonParkablePayloadTransactions<I> {
    /// Wraps a payload iterator without adding parking support.
    pub const fn new(inner: I) -> Self {
        Self { inner }
    }
}

impl<I> PayloadTransactions for NonParkablePayloadTransactions<I>
where
    I: PayloadTransactions,
{
    type Transaction = I::Transaction;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        self.inner.next(ctx)
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);
    }
}

impl<I> ParkablePayloadTransactions for NonParkablePayloadTransactions<I>
where
    I: PayloadTransactions,
    I::Transaction: PoolTransaction,
{
    fn park_current(&mut self) -> bool {
        false
    }

    fn mark_current_committed(&mut self) {}

    fn promote(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }

    fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
        false
    }
}

/// Converts a parkable best iterator into the payload-transaction interface used by the builder.
#[derive(derive_more::Debug)]
pub struct ParkableBestPayloadTransactions<T>
where
    T: BasePooledTx,
{
    #[debug(skip)]
    inner: Box<dyn ParkableBestTransactions<T>>,
    #[debug("{:?}", current.is_some())]
    current: Option<Arc<ValidPoolTransaction<T>>>,
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
    type Transaction = T;

    fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
        debug_assert!(
            self.current.is_none(),
            "previous transaction must be lifecycle-managed before next"
        );
        let transaction = self.inner.next()?;
        self.current = Some(Arc::clone(&transaction));
        Some(transaction.transaction.clone())
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
    fn park_current(&mut self) -> bool {
        let Some(transaction) = self.current.take() else {
            return false;
        };
        self.inner.park(&transaction);
        true
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
}
