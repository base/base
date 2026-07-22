//! Pluggable source of candidate transactions for the flashblocks build loop.
//!
//! When building a (flash)block the builder drains a priority-ordered stream of candidate
//! transactions. Historically that stream was always the pool's
//! [`TransactionPool::best_transactions_with_attributes`] iterator, constructed inline in the
//! build loop. [`CandidateSource`] extracts that construction behind a trait so the stream can be
//! supplied by an alternative implementation without forking the build loop.
//!
//! A source captures whatever it draws candidates from at construction time (the pool, an external
//! block-builder API, a pre-sorted bundle, a test fixture, ...) and yields a stream on demand. The
//! builder holds one and (re)builds its iterator once per flashblock. [`DefaultCandidateSource`] is
//! the default: it captures the transaction pool and reproduces the builder's historical behavior
//! exactly.

use std::sync::Arc;

use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};

/// The boxed, priority-ordered best-transactions iterator drained by the flashblocks builder.
///
/// This matches the return type of [`TransactionPool::best_transactions_with_attributes`], so a
/// [`CandidateSource`] implementation can hand its stream straight into the build loop's iterator
/// adapter with no change to the loop's concrete iterator type.
pub type BoxedBestTransactions<T> = Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>;

/// Supplies the priority-ordered candidate transaction stream for the next (flash)block.
///
/// A source captures its underlying data at construction time, so the builder need not thread the
/// transaction pool (or any other backing store) through on every call — an alternative source that
/// draws from, say, an external API is not forced to accept a `&Pool` it would ignore. The builder
/// calls [`CandidateSource::best_transactions`] once per flashblock to (re)build its iterator.
pub trait CandidateSource: Send + Sync + std::fmt::Debug {
    /// The pool transaction type yielded by this source.
    type Transaction: PoolTransaction;

    /// Produce the priority-ordered candidate stream for the given fee attributes.
    fn best_transactions(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<Self::Transaction>;
}

/// The default candidate source: the pool's priority-ordered best transactions.
///
/// This captures the transaction pool and reproduces the builder's pre-seam behavior byte-for-byte
/// — it simply forwards to [`TransactionPool::best_transactions_with_attributes`].
#[derive(Debug, Clone)]
pub struct DefaultCandidateSource<Pool> {
    /// The transaction pool candidates are drawn from.
    pool: Pool,
}

impl<Pool> DefaultCandidateSource<Pool> {
    /// Create a [`DefaultCandidateSource`] drawing candidates from `pool`.
    pub const fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl<Pool> CandidateSource for DefaultCandidateSource<Pool>
where
    Pool: TransactionPool + std::fmt::Debug,
{
    type Transaction = Pool::Transaction;

    fn best_transactions(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> BoxedBestTransactions<Self::Transaction> {
        self.pool.best_transactions_with_attributes(attributes)
    }
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;

    use reth_transaction_pool::test_utils::MockTransaction;

    use super::{BestTransactionsAttributes, BoxedBestTransactions, CandidateSource, PoolTransaction};

    /// A [`CandidateSource`] that draws from no transaction pool at all: it captures nothing and
    /// yields an empty stream. Its existence proves the seam is genuinely pluggable — an alternative
    /// source is implementable without a pool and is never handed a `&Pool` it would ignore.
    #[derive(Debug)]
    struct EmptyCandidateSource<T>(PhantomData<fn() -> T>);

    impl<T> CandidateSource for EmptyCandidateSource<T>
    where
        T: PoolTransaction,
    {
        type Transaction = T;

        fn best_transactions(
            &self,
            _attributes: BestTransactionsAttributes,
        ) -> BoxedBestTransactions<Self::Transaction> {
            Box::new(std::iter::empty())
        }
    }

    #[test]
    fn alternative_source_needs_no_pool() {
        let source = EmptyCandidateSource::<MockTransaction>(PhantomData);
        let mut stream = source.best_transactions(BestTransactionsAttributes::new(0, None));
        assert!(stream.next().is_none());
    }
}
