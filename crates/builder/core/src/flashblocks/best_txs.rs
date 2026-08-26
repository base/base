//! Flashblocks adapters for parkable best-transaction iterators.

use std::{collections::HashSet, marker::PhantomData, sync::Arc};

use alloy_primitives::{Address, TxHash};
use base_execution_txpool::{BasePooledTx, ParkableBestTransactions};
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};

use crate::{BuilderMetrics, RejectionCache};

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

/// An adapter that skips transactions already committed or permanently rejected by flashblocks.
pub struct BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    inner: I,
    // Transactions that were already committed to the state. Using them again would cause NonceTooLow
    // so we skip them
    committed_transactions: HashSet<TxHash>,
    // Shared cross-block rejection cache (survives across blocks, TTL-bounded)
    rejection_cache: RejectionCache,
    // Identity of the transaction most recently returned to the build loop.
    current_transaction: Option<(TxHash, Address, u64)>,
    transaction: PhantomData<T>,
}

impl<T, I> std::fmt::Debug for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BestFlashblocksTxs")
            .field("committed_transactions", &self.committed_transactions)
            .field("rejection_cache_size", &self.rejection_cache.entry_count())
            .finish_non_exhaustive()
    }
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    /// Creates a new [`BestFlashblocksTxs`] wrapping the given payload transaction iterator.
    pub fn new(inner: I, rejection_cache: RejectionCache) -> Self {
        Self {
            inner,
            committed_transactions: Default::default(),
            rejection_cache,
            current_transaction: None,
            transaction: PhantomData,
        }
    }

    /// Replaces current iterator with new one. We use it on new flashblock building, to refresh
    /// priority boundaries
    pub fn refresh_iterator(&mut self, inner: I) {
        self.inner = inner;
        self.current_transaction = None;
    }

    /// Remove transaction from next iteration since it is already in the state
    pub fn mark_committed(&mut self, txs: &[TxHash]) {
        self.committed_transactions.extend(txs);
    }

    /// Mark transactions as permanently rejected. They will be skipped in all
    /// subsequent flashblocks within this block and across future blocks via
    /// the shared rejection cache.
    pub fn mark_rejected(&mut self, tx_hashes: &[TxHash]) {
        for hash in tx_hashes {
            self.rejection_cache.insert(*hash);
        }
        BuilderMetrics::rejection_cache_insertions().increment(tx_hashes.len() as u64);
        BuilderMetrics::rejection_cache_size().set(self.rejection_cache.entry_count() as f64);
    }
}

impl<T, I> PayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    type Transaction = T;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        loop {
            let tx = self.inner.next(ctx)?;
            let hash = *tx.hash();
            self.current_transaction = Some((hash, tx.sender(), tx.nonce()));

            if self.committed_transactions.contains(&hash) {
                self.inner.mark_current_committed();
                self.current_transaction = None;
                continue;
            }

            if self.rejection_cache.contains_key(&hash) {
                BuilderMetrics::rejection_cache_hits().increment(1);
                // Only intrinsically invalid transactions enter this cache. Their nonce-lane
                // descendants cannot execute across the resulting gap, so exclude the lane for
                // this iterator rather than treating the rejected head as committed.
                self.inner.mark_invalid(tx.sender(), tx.nonce());
                self.current_transaction = None;
                continue;
            }

            return Some(tx);
        }
    }

    /// Proxy to inner iterator
    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        let matches_current =
            self.current_transaction.is_some_and(|(_, current_sender, current_nonce)| {
                current_sender == sender && current_nonce == nonce
            });
        debug_assert!(matches_current, "mark_invalid must identify the current transaction");
        if !matches_current {
            return;
        }
        self.inner.mark_invalid(sender, nonce);
        self.current_transaction = None;
    }
}

impl<T, I> ParkablePayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    fn park_current(&mut self) -> bool {
        let parked = self.inner.park_current();
        if parked {
            self.current_transaction = None;
        }
        parked
    }

    fn mark_current_committed(&mut self) {
        self.inner.mark_current_committed();
        if let Some((transaction_hash, _, _)) = self.current_transaction.take() {
            self.committed_transactions.insert(transaction_hash);
        }
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.discard_parked(transaction_hash)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_consensus::Transaction;
    use alloy_eips::eip1559::MIN_PROTOCOL_BASE_FEE;
    use alloy_primitives::Address;
    use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, PoolTransaction,
        pool::PendingPool,
        test_utils::{MockTransaction, MockTransactionFactory},
    };

    use crate::{
        RejectionCache,
        flashblocks::best_txs::{BestFlashblocksTxs, ParkablePayloadTransactions},
    };

    fn test_rejection_cache() -> RejectionCache {
        RejectionCache::new(1000, Duration::from_secs(60))
    }

    #[test]
    fn test_simple_case() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        let mut f = MockTransactionFactory::default();

        // Add 3 regular transaction
        let tx_1 = f.create_eip1559();
        let tx_2 = f.create_eip1559();
        let tx_3 = f.create_eip1559();
        pool.add_transaction(Arc::new(tx_1), 0);
        pool.add_transaction(Arc::new(tx_2), 0);
        pool.add_transaction(Arc::new(tx_3), 0);

        // Create iterator
        let mut iterator = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(pool.best()),
            test_rejection_cache(),
        );
        // ### First flashblock
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));
        // Accept first tx
        let tx1 = iterator.next(()).unwrap();
        // Invalidate second tx
        let tx2 = iterator.next(()).unwrap();
        iterator.mark_invalid(tx2.sender(), tx2.nonce());
        // Accept third tx
        let tx3 = iterator.next(()).unwrap();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_committed(&[*tx1.hash(), *tx3.hash()]);

        // ### Second flashblock
        // It should not return txs 1 and 3, but should return 2
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));
        let tx2 = iterator.next(()).unwrap();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_committed(&[*tx2.hash()]);

        // ### Third flashblock
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
    }

    #[test]
    fn non_parkable_iterator_reports_parking_capacity_miss() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        let mut f = MockTransactionFactory::default();
        pool.add_transaction(Arc::new(f.create_eip1559()), 0);

        let mut iterator = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(pool.best()),
            test_rejection_cache(),
        );
        assert!(iterator.next(()).is_some());
        assert!(
            !iterator.park_current(),
            "BestPayloadTransactions cannot park, so the builder must emit BUILDER_REJECTED"
        );
    }

    /// This test simulates the nonce-chain gating fix across flashblock boundaries.
    ///
    /// Scenario (based on real Base Mainnet block 41628995):
    /// - Sender A has `TX_A` (nonce 0, LOW tip) and `TX_B` (nonce 1, HIGH tip) in the pool
    /// - Sender B has `TX_C` (MEDIUM tip)
    ///
    /// `TX_A` is in the mempool, `TX_B` and `TX_C` arrive later after the first flashblock has
    /// started building already.
    ///
    /// - In flashblock 1, `TX_A` gets consumed (`TX_B` unlocks after `TX_A`)
    /// - Only `TX_A` is marked as committed (simulating flashblock timer expiring)
    /// - In flashblock 2, `TX_B` (HIGH tip) should come before `TX_C` (MEDIUM tip)
    ///
    /// Expected: `TX_B` (100 gwei) before `TX_C` (10 gwei) in flashblock 2.
    ///
    /// The upstream reth PR (<https://github.com/paradigmxyz/reth/pull/21765>) that added
    /// `prune_transactions` to the pool trait has been merged. The production fix calls
    /// `pool.prune_transactions` after `mark_committed` between flashblocks, which removes
    /// the already-executed `TX_A` from the pool so the iterator sees the correct priority
    /// ordering. This test simulates that behavior by recreating the pool without `TX_A`
    /// and verifies that `TX_B` (100 gwei) is correctly ordered before `TX_C` (10 gwei).
    #[test]
    fn test_nonce_chain_gating_bug_across_flashblocks() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        let mut f = MockTransactionFactory::default();

        let sender_a = Address::random();
        let sender_b = Address::random();

        let tx_a = MockTransaction::eip1559()
            .with_sender(sender_a)
            .with_nonce(0)
            .with_priority_fee(1_000_000_000) // 1 gwei - LOW
            .with_max_fee(100_000_000_000);

        let tx_b = MockTransaction::eip1559()
            .with_sender(sender_a)
            .with_nonce(1)
            .with_priority_fee(100_000_000_000) // 100 gwei - HIGH (depends on TX_A)
            .with_max_fee(200_000_000_000);

        let tx_c = MockTransaction::eip1559()
            .with_sender(sender_b)
            .with_nonce(0)
            .with_priority_fee(10_000_000_000) // 10 gwei - MEDIUM
            .with_max_fee(100_000_000_000);

        pool.add_transaction(Arc::new(f.validated(tx_a.clone())), 0);

        // === FLASHBLOCK 1 ===
        let mut iterator = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(pool.best()),
            test_rejection_cache(),
        );

        // Simulate: Flashblock 1 starts building
        // Start consuming txns from the txpool
        let first = iterator.next(()).unwrap();
        assert_eq!(first.sender(), sender_a, "First should be TX_A (1 gwei)");

        // TX_B and TX_C arrive late, but we have already yielded lower-priority transactions
        // from the iterator, so these do not immediately get added to the best txns
        pool.add_transaction(Arc::new(f.validated(tx_b.clone())), 0);
        pool.add_transaction(Arc::new(f.validated(tx_c.clone())), 0);
        assert_eq!(iterator.next(()), None);

        // Simulate: flashblock 1 is complete after TX_A was executed
        iterator.mark_committed(&[*tx_a.hash()]);
        // Simulate pool.prune_transactions by recreating the pool without TX_A
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        pool.add_transaction(Arc::new(f.validated(tx_b)), 0);
        pool.add_transaction(Arc::new(f.validated(tx_c)), 0);

        // === FLASHBLOCK 2 ===
        // We refresh the iterator with the latest best transactions
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));

        // Now, theoretically, TX_A has already been executed, so
        // TX_B should be the best txn and TX_C the second best
        // Expected: TX_B (100 gwei) first, TX_C (10 gwei) second
        let fb2_first = iterator.next(()).unwrap();
        let fb2_second = iterator.next(()).unwrap();

        assert_eq!(fb2_first.sender(), sender_a);
        assert_eq!(fb2_second.sender(), sender_b);
        assert!(
            fb2_second.effective_tip_per_gas(MIN_PROTOCOL_BASE_FEE)
                < fb2_first.effective_tip_per_gas(MIN_PROTOCOL_BASE_FEE)
        );
    }

    /// Reproduces the nonce-chain queuing bug caused by `prune_transactions`.
    ///
    /// After FB1 prunes executed nonce-0 txs, the pool's on-chain nonce view is stale
    /// (block not sealed), so nonce-1 txs from the same senders land in `queued`
    /// instead of `pending`, making them invisible to FB2+.
    #[tokio::test]
    async fn test_prune_transactions_causes_nonce_chain_queuing() {
        use alloy_primitives::{Address, U256};
        use reth_execution_types::ChangedAccount;
        use reth_transaction_pool::{
            BestTransactionsAttributes, TransactionOrigin, TransactionPool, TransactionPoolExt,
            test_utils::testing_pool,
        };

        let pool = testing_pool();

        let senders: Vec<Address> = (0..3).map(|_| Address::random()).collect();

        // All senders submit nonce-0 txs
        for sender in &senders {
            let tx = MockTransaction::eip1559()
                .with_sender(*sender)
                .with_nonce(0)
                .with_gas_limit(21_000)
                .with_priority_fee(5_000_000_000)
                .with_max_fee(100_000_000_000);
            pool.add_transaction(TransactionOrigin::External, tx).await.unwrap();
        }
        assert_eq!(pool.pool_size().pending, 3);

        // Simulate FB1: consume all nonce-0 txs, then prune them
        let best_attrs = BestTransactionsAttributes::new(0, None);
        let mut best_iter = pool.best_transactions_with_attributes(best_attrs);
        let mut executed_hashes = Vec::new();
        for tx in best_iter.by_ref() {
            executed_hashes.push(*tx.hash());
        }
        drop(best_iter);
        assert_eq!(executed_hashes.len(), 3);
        pool.prune_transactions(executed_hashes);
        assert_eq!(pool.pool_size().pending, 0);

        // Senders submit nonce-1 txs (arrive between FB1 and FB2)
        for sender in &senders {
            let tx = MockTransaction::eip1559()
                .with_sender(*sender)
                .with_nonce(1)
                .with_gas_limit(21_000)
                .with_priority_fee(5_000_000_000)
                .with_max_fee(100_000_000_000);
            pool.add_transaction(TransactionOrigin::External, tx).await.unwrap();
        }

        // Bug: nonce-1 txs are queued (nonce gap) because pool still thinks on-chain nonce is 0
        assert_eq!(pool.pool_size().pending, 0, "nonce-1 txs should be queued without fix");
        assert_eq!(pool.pool_size().queued, 3, "nonce-1 txs land in queued due to stale nonce");

        // Fix: update_accounts corrects the pool's nonce view, promoting queued -> pending.
        // U256::MAX balance is fine here — testing_pool has no revm state to read from.
        // Production code uses state.basic(address) for real balances.
        let changed_accounts: Vec<ChangedAccount> = senders
            .iter()
            .map(|&address| ChangedAccount { address, nonce: 1, balance: U256::MAX })
            .collect();
        pool.update_accounts(changed_accounts);
        assert_eq!(pool.pool_size().pending, 3, "nonce-1 txs should be pending after fix");
        assert_eq!(pool.pool_size().queued, 0, "no txs should be queued after fix");

        // FB2's iterator must see all 3 nonce-1 txs
        let mut fb2_iter = pool.best_transactions_with_attributes(best_attrs);
        let mut count = 0;
        while fb2_iter.next().is_some() {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    /// Rejected transactions are skipped across flashblock boundaries within the same block.
    #[test]
    fn test_rejected_txs_persist_across_refresh() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        let mut f = MockTransactionFactory::default();

        let tx_1 = f.create_eip1559();
        let tx_2 = f.create_eip1559();
        let tx_3 = f.create_eip1559();
        let tx_2_hash = *tx_2.hash();
        pool.add_transaction(Arc::new(tx_1), 0);
        pool.add_transaction(Arc::new(tx_2), 0);
        pool.add_transaction(Arc::new(tx_3), 0);

        let mut iterator = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(pool.best()),
            test_rejection_cache(),
        );

        // FB1: consume first tx, reject second permanently
        let _tx1 = iterator.next(()).unwrap();
        let _tx2 = iterator.next(()).unwrap();
        iterator.mark_rejected(&[tx_2_hash]);
        let _tx3 = iterator.next(()).unwrap();
        assert!(iterator.next(()).is_none());

        // FB2: refresh iterator — tx2 should still be skipped
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));
        let mut seen_hashes = Vec::new();
        while let Some(tx) = iterator.next(()) {
            seen_hashes.push(*tx.hash());
        }
        assert!(!seen_hashes.contains(&tx_2_hash), "rejected tx should not reappear after refresh");
        assert_eq!(seen_hashes.len(), 2, "only non-rejected txs should appear");
    }

    /// Rejected transactions in the shared cache are skipped by a new iterator instance
    /// (simulating cross-block persistence).
    #[test]
    fn test_rejection_cache_persists_across_blocks() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        let mut f = MockTransactionFactory::default();

        let tx_1 = f.create_eip1559();
        let tx_2 = f.create_eip1559();
        let tx_2_hash = *tx_2.hash();
        pool.add_transaction(Arc::new(tx_1), 0);
        pool.add_transaction(Arc::new(tx_2), 0);

        let cache = test_rejection_cache();

        // Block 1: reject tx_2
        let mut iter1 =
            BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()), cache.clone());
        let _tx1 = iter1.next(()).unwrap();
        let _tx2 = iter1.next(()).unwrap();
        iter1.mark_rejected(&[tx_2_hash]);

        // Block 2: new iterator, same cache — tx_2 should be skipped
        let mut iter2 = BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()), cache);
        let mut seen_hashes = Vec::new();
        while let Some(tx) = iter2.next(()) {
            seen_hashes.push(*tx.hash());
        }
        assert!(
            !seen_hashes.contains(&tx_2_hash),
            "tx rejected in block 1 should be skipped in block 2"
        );
        assert_eq!(seen_hashes.len(), 1, "only non-rejected tx should appear");
    }

    #[test]
    fn rejection_cache_hit_excludes_nonce_descendants() {
        let sender = Address::random();
        let other_sender = Address::random();
        let rejected =
            MockTransaction::eip1559().with_sender(sender).with_nonce(0).with_priority_fee(3);
        let descendant =
            MockTransaction::eip1559().with_sender(sender).with_nonce(1).with_priority_fee(2);
        let other =
            MockTransaction::eip1559().with_sender(other_sender).with_nonce(0).with_priority_fee(1);
        let rejected_hash = *rejected.hash();
        let other_hash = *other.hash();
        let mut factory = MockTransactionFactory::default();
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        pool.add_transaction(Arc::new(factory.validated(rejected)), 0);
        pool.add_transaction(Arc::new(factory.validated(descendant)), 0);
        pool.add_transaction(Arc::new(factory.validated(other)), 0);
        let cache = test_rejection_cache();
        cache.insert(rejected_hash);
        let mut iterator =
            BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()), cache);

        let yielded_hashes: Vec<_> =
            std::iter::from_fn(|| iterator.next(())).map(|tx| *tx.hash()).collect();

        assert_eq!(yielded_hashes, vec![other_hash]);
    }

    /// A rejected transaction becomes eligible again after the cache TTL expires.
    #[test]
    fn test_rejected_tx_eligible_after_ttl_expiry() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<MockTransaction>::default());
        let mut f = MockTransactionFactory::default();

        let tx_1 = f.create_eip1559();
        let tx_2 = f.create_eip1559();
        let tx_2_hash = *tx_2.hash();
        pool.add_transaction(Arc::new(tx_1), 0);
        pool.add_transaction(Arc::new(tx_2), 0);

        // TTL is short, 1ms
        let cache = RejectionCache::new(1000, Duration::from_millis(1));

        // Reject tx_2
        let mut iter1 =
            BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()), cache.clone());
        let _tx1 = iter1.next(()).unwrap();
        let _tx2 = iter1.next(()).unwrap();
        iter1.mark_rejected(&[tx_2_hash]);

        // Wait for TTL to expire and flush pending evictions
        std::thread::sleep(Duration::from_millis(50));
        cache.run_pending_tasks();

        // New iterator — tx_2 should be back
        let mut iter2 = BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()), cache);
        let mut seen_hashes = Vec::new();
        while let Some(tx) = iter2.next(()) {
            seen_hashes.push(*tx.hash());
        }
        assert!(seen_hashes.contains(&tx_2_hash), "tx should be eligible again after TTL expiry");
        assert_eq!(seen_hashes.len(), 2, "both txs should appear");
    }
}
