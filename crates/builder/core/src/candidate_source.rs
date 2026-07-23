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

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;
    use reth_transaction_pool::{
        error::InvalidPoolTransactionError,
        test_utils::{MockTransaction, MockTransactionFactory, MockValidTx},
    };

    use super::*;

    /// A minimal [`BestTransactions`] backed by a [`Vec`], for exercising [`CandidateSource`]
    /// implementations without standing up a real pool. The pool-update and blob-skipping hooks are
    /// no-ops — these tests only care about which transactions flow through the stream.
    #[derive(Debug)]
    struct VecBest(std::vec::IntoIter<Arc<MockValidTx>>);

    impl Iterator for VecBest {
        type Item = Arc<MockValidTx>;

        fn next(&mut self) -> Option<Self::Item> {
            self.0.next()
        }
    }

    impl BestTransactions for VecBest {
        fn mark_invalid(&mut self, _tx: &Self::Item, _kind: InvalidPoolTransactionError) {}
        fn no_updates(&mut self) {}
        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    /// An alternative [`CandidateSource`] that transforms the incoming pool stream in place — here
    /// dropping even-nonce transactions — rather than supplying a stream of its own. Proves the seam
    /// lets a source wrap/filter `pool_best` while preserving the loop's iterator type.
    #[derive(Debug)]
    struct OddNonceOnly;

    impl CandidateSource<MockTransaction> for OddNonceOnly {
        fn best_transactions(
            &self,
            pool_best: BoxedBestTransactions<MockTransaction>,
            _attributes: BestTransactionsAttributes,
        ) -> BoxedBestTransactions<MockTransaction> {
            let kept = pool_best.filter(|tx| tx.nonce() % 2 == 1).collect::<Vec<_>>();
            Box::new(VecBest(kept.into_iter()))
        }
    }

    fn pool_stream(nonces: &[u64]) -> (Vec<TxHash>, BoxedBestTransactions<MockTransaction>) {
        let mut factory = MockTransactionFactory::default();
        let txs = nonces
            .iter()
            .map(|&n| factory.validated_arc(MockTransaction::eip1559().with_nonce(n).rng_hash()))
            .collect::<Vec<_>>();
        let hashes = txs.iter().map(|tx| *tx.hash()).collect();
        (hashes, Box::new(VecBest(txs.into_iter())))
    }

    #[test]
    fn default_source_passes_the_pool_stream_through_unchanged() {
        let (expected, pool_best) = pool_stream(&[0, 1, 2]);
        let out = DefaultCandidateSource
            .best_transactions(pool_best, BestTransactionsAttributes::new(0, None));
        let got = out.map(|tx| *tx.hash()).collect::<Vec<_>>();
        assert_eq!(got, expected, "the default source must yield the pool stream unchanged");
    }

    #[test]
    fn alternative_source_can_filter_the_pool_stream() {
        let (all, pool_best) = pool_stream(&[0, 1, 2, 3]);
        let out =
            OddNonceOnly.best_transactions(pool_best, BestTransactionsAttributes::new(0, None));
        let got = out.map(|tx| *tx.hash()).collect::<Vec<_>>();
        // Only the odd-nonce transactions (indices 1 and 3) survive the transform.
        assert_eq!(got, vec![all[1], all[3]]);
    }
}
