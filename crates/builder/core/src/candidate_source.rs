//! Pluggable source of candidate transactions for the flashblocks build loop.
//!
//! When building a (flash)block the builder drains a priority-ordered stream of candidate
//! transactions. Historically that stream was always the pool's
//! [`TransactionPool::best_transactions_with_attributes`](reth_transaction_pool::TransactionPool::best_transactions_with_attributes)
//! iterator, constructed inline in the build loop. [`CandidateSource`] lets that stream be
//! transformed by an alternative implementation — which receives the pool's best transactions and
//! returns the stream the loop should drain — without forking the build loop.
//!
//! [`DefaultCandidateSource`] is the default and returns the pool's stream unchanged, reproducing
//! the builder's historical behavior exactly.

use std::sync::Arc;

use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, ValidPoolTransaction,
};

/// The boxed, priority-ordered best-transactions iterator drained by the flashblocks builder.
///
/// This matches the return type of
/// [`TransactionPool::best_transactions_with_attributes`](reth_transaction_pool::TransactionPool::best_transactions_with_attributes),
/// so a [`CandidateSource`] can receive and return this stream without the loop's concrete iterator
/// type changing.
pub type BoxedBestTransactions<T> = Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>;

/// Transforms the priority-ordered candidate transaction stream drained for the next (flash)block.
///
/// The builder calls [`CandidateSource::best_transactions`] once per flashblock, passing the pool's
/// current best transactions and receiving the stream to drain. The default
/// [`DefaultCandidateSource`] returns the pool stream unchanged; an alternative implementation may
/// return a different stream (for example one that composes the pool stream with transactions from
/// an additional source) while preserving the loop's contract.
pub trait CandidateSource<T>: Send + Sync + std::fmt::Debug
where
    T: PoolTransaction,
{
    /// Given the pool's priority-ordered best transactions, produce the stream the build loop drains.
    ///
    /// `attributes` are the same fee attributes used to build `pool_best`, provided so an
    /// implementation can rank any transactions it contributes on the same basis.
    fn best_transactions(
        &self,
        pool_best: BoxedBestTransactions<T>,
        attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<T>;
}

/// The default candidate source: the pool's priority-ordered best transactions, unchanged.
///
/// This reproduces the builder's pre-seam behavior byte-for-byte — it returns `pool_best` as-is.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCandidateSource;

impl<T> CandidateSource<T> for DefaultCandidateSource
where
    T: PoolTransaction,
{
    fn best_transactions(
        &self,
        pool_best: BoxedBestTransactions<T>,
        _attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<T> {
        pool_best
    }
}
