//! Pluggable source of candidate transactions for the flashblocks build loop.
//!
//! When building a (flash)block the builder drains a priority-ordered stream of candidate
//! transactions. Historically that stream was always the pool's
//! [`TransactionPool::best_transactions_with_attributes`] iterator, constructed inline in the
//! build loop. [`CandidateSource`] extracts that construction behind a trait so the stream can be
//! supplied by an alternative implementation without forking the build loop.
//!
//! [`DefaultCandidateSource`] is the default and reproduces the historical behavior exactly.

use std::sync::Arc;

use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool, ValidPoolTransaction,
};

/// The boxed, priority-ordered best-transactions iterator drained by the flashblocks builder.
///
/// This matches the return type of [`TransactionPool::best_transactions_with_attributes`], so a
/// [`CandidateSource`] implementation can hand its stream straight into the build loop's iterator
/// adapter with no change to the loop's concrete iterator type.
pub type BoxedBestTransactions<T> =
    Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>;

/// Supplies the priority-ordered candidate transaction stream for the next (flash)block.
///
/// The builder calls [`CandidateSource::best_transactions`] once per flashblock to (re)build its
/// iterator. The default [`DefaultCandidateSource`] returns the pool's best transactions; an
/// alternative implementation may supply a different stream while preserving the loop's contract.
pub trait CandidateSource<Pool>: Send + Sync + std::fmt::Debug
where
    Pool: TransactionPool,
{
    /// Produce the priority-ordered candidate stream for the given fee attributes.
    fn best_transactions(
        &self,
        pool: &Pool,
        attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<Pool::Transaction>;
}

/// The default candidate source: the pool's priority-ordered best transactions.
///
/// This reproduces the builder's pre-seam behavior byte-for-byte — it simply forwards to
/// [`TransactionPool::best_transactions_with_attributes`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCandidateSource;

impl<Pool> CandidateSource<Pool> for DefaultCandidateSource
where
    Pool: TransactionPool,
{
    fn best_transactions(
        &self,
        pool: &Pool,
        attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<Pool::Transaction> {
        pool.best_transactions_with_attributes(attributes)
    }
}
