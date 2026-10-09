//! Flashblocks adapters for parkable best-transaction iterators.

use std::{collections::HashSet, marker::PhantomData, time::Instant};

use alloy_primitives::{Address, TxHash, map::B256Set};
use base_execution_payload_builder::{ParkablePayloadTransactions, ParkedPredicateIndex};
use base_execution_txpool::{BasePooledTx, ValidityConditions, ValidityPredicate};
use reth_payload_util::PayloadTransactions;
use revm::state::EvmState;

use crate::{
    BuilderMetrics, RejectionCache, RestingPayloadTransactions, RestingPredicateMode, RestingStats,
};

/// An adapter that skips transactions already committed or permanently rejected by flashblocks.
///
/// It also holds back validity transactions resting under an unchanged predicate, see
/// [`RestingPayloadTransactions`]. A resting transaction is parked in the inner iterator, so its
/// nonce lane stays blocked, and is promoted back at its priority position once a commit changes
/// the state its predicate reads.
pub struct BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
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
    resting_predicate_mode: RestingPredicateMode,
    // Transactions resting in this block, indexed by the predicate last found unsatisfied.
    resting: ParkedPredicateIndex<()>,
    // Resting transactions parked by `next` in the current inner iterator. Transactions the build
    // loop parked are woken by its own predicate index instead.
    parked_resting: B256Set,
    resting_stats: RestingStats,
    transaction: PhantomData<T>,
}

impl<T, I> std::fmt::Debug for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BestFlashblocksTxs")
            .field("committed_transactions", &self.committed_transactions)
            .field("rejection_cache_size", &self.rejection_cache.entry_count())
            .field("resting_predicate_mode", &self.resting_predicate_mode)
            .field("parked_resting", &self.parked_resting.len())
            .finish_non_exhaustive()
    }
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    /// Creates a new [`BestFlashblocksTxs`] wrapping the given payload transaction iterator.
    pub fn new(inner: I, rejection_cache: RejectionCache) -> Self {
        Self {
            inner,
            committed_transactions: Default::default(),
            rejection_cache,
            current_transaction: None,
            resting_predicate_mode: RestingPredicateMode::Off,
            resting: ParkedPredicateIndex::default(),
            parked_resting: B256Set::default(),
            resting_stats: RestingStats::default(),
            transaction: PhantomData,
        }
    }

    /// Sets whether resting validity transactions are tracked and held back.
    #[must_use]
    pub const fn with_resting_predicate_mode(mut self, mode: RestingPredicateMode) -> Self {
        self.resting_predicate_mode = mode;
        self
    }

    /// Replaces current iterator with new one. We use it on new flashblock building, to refresh
    /// priority boundaries
    pub fn refresh_iterator(&mut self, inner: I) {
        self.inner = inner;
        self.current_transaction = None;
        self.parked_resting.clear();
    }

    /// Remove transaction from next iteration since it is already in the state
    pub fn mark_committed(&mut self, txs: &[TxHash]) {
        self.committed_transactions.extend(txs);
    }

    /// Mark transactions as permanently rejected. They will be skipped in all
    /// subsequent flashblocks within this block and across future blocks via
    /// the shared rejection cache.
    pub fn mark_rejected(&mut self, tx_hashes: &[TxHash]) {
        self.rejection_cache.mark_rejected(tx_hashes);
        BuilderMetrics::rejection_cache_insertions().increment(tx_hashes.len() as u64);
        BuilderMetrics::rejection_cache_size().set(self.rejection_cache.entry_count() as f64);
    }
}

impl<T, I> PayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
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

            if self.rejection_cache.is_rejected(&hash) {
                BuilderMetrics::rejection_cache_hits().increment(1);
                // Only intrinsically invalid transactions enter this cache. Their nonce-lane
                // descendants cannot execute across the resulting gap, so exclude the lane for
                // this iterator rather than treating the rejected head as committed.
                self.inner.mark_invalid(tx.sender(), tx.nonce());
                self.current_transaction = None;
                continue;
            }

            if self.resting_predicate_mode.is_enforced()
                && !self.resting.is_empty()
                && !tx.validity_conditions().is_empty()
            {
                let started = Instant::now();
                let resting = self.is_resting(hash, tx.validity_conditions());
                if resting {
                    self.inner.park_current();
                    self.parked_resting.insert(hash);
                    self.current_transaction = None;
                    self.resting_stats.parked += 1;
                }
                self.resting_stats.duration += started.elapsed();
                if resting {
                    continue;
                }
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
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    fn park_current(&mut self) {
        self.inner.park_current();
        self.current_transaction = None;
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

impl<T, I> RestingPayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: ParkablePayloadTransactions<Transaction = T>,
{
    /// Flashblock-index predicates are not recorded because the index changes between
    /// flashblocks without any commit.
    fn rest(&mut self, transaction_hash: TxHash, predicate: &ValidityPredicate) {
        if !self.resting_predicate_mode.is_enabled()
            || matches!(predicate, ValidityPredicate::FlashblockIndex { .. })
        {
            return;
        }
        self.resting.park(transaction_hash, (), predicate.clone());
    }

    fn record_committed_state(&mut self, state: &EvmState) {
        if self.resting.is_empty() {
            return;
        }
        for transaction_hash in self.resting.affected_by_state(state).affected_transactions {
            self.resting.remove(transaction_hash);
            if self.parked_resting.remove(&transaction_hash) {
                self.inner.promote(transaction_hash);
            }
        }
    }

    /// A hash re-added to the pool with a batch that no longer contains the recorded predicate
    /// does not rest.
    fn is_resting(&self, transaction_hash: TxHash, predicates: &ValidityConditions) -> bool {
        self.resting.predicate(transaction_hash).is_some_and(|blocker| predicates.contains(blocker))
    }

    fn take_resting_stats(&mut self) -> RestingStats {
        std::mem::take(&mut self.resting_stats)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_consensus::{SignableTransaction, Transaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Signature, TxHash, TxKind, U256};
    use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
    use base_execution_txpool::{
        BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
        ValidityPredicate,
    };
    use reth_payload_util::PayloadTransactions;
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
        pool::PendingPool,
    };
    use revm::state::{Account, EvmState};

    use crate::{
        BestFlashblocksTxs, ParkableBestPayloadTransactions, ParkablePayloadTransactions,
        RejectionCache, RestingPayloadTransactions, RestingPredicateMode,
    };

    type Ordering = BaseOrdering<BasePooledTransaction>;
    type Parkable = ParkableBestPayloadTransactions<BasePooledTransaction>;

    const WATCHED: Address = Address::repeat_byte(0xaa);
    const UNRELATED: Address = Address::repeat_byte(0xbb);

    fn test_rejection_cache() -> RejectionCache {
        RejectionCache::new(1000, Duration::from_secs(60))
    }

    fn sender_address(sender: u64) -> Address {
        Address::from_word(U256::from(sender + 1).into())
    }

    fn transaction(
        sender: u64,
        nonce: u64,
        priority_fee: u128,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        validity_transaction(sender, nonce, priority_fee, Vec::new())
    }

    fn validity_transaction(
        sender: u64,
        nonce: u64,
        priority_fee: u128,
        predicates: Vec<ValidityPredicate>,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: priority_fee + 100,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(Address::ZERO),
            // Distinguishes otherwise identical transactions from different senders, which share
            // the test signature.
            value: U256::from(sender),
            ..Default::default()
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
        let encoded_length = envelope.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender_address(sender)),
            encoded_length,
        )
        .with_validity_predicates(predicates);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(sender.into(), nonce),
            transaction,
            propagate: true,
            timestamp: std::time::Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    fn pending_pool(
        transactions: &[Arc<ValidPoolTransaction<BasePooledTransaction>>],
    ) -> PendingPool<Ordering> {
        let mut pool = PendingPool::new(Ordering::coinbase_tip());
        for transaction in transactions {
            pool.add_transaction(Arc::clone(transaction), 0);
        }
        pool
    }

    /// Builds the production lane-parking iterator over a snapshot of `pool`.
    fn parkable(
        pool: &PendingPool<Ordering>,
    ) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
        ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
            pool.best(),
            Ordering::coinbase_tip(),
            0,
        )))
    }

    /// Drains `iterator`, marking each yielded transaction invalid for this scan only, and
    /// returns the yielded hashes.
    fn drain_without_including<I>(iterator: &mut I) -> Vec<TxHash>
    where
        I: PayloadTransactions<Transaction = BasePooledTransaction>,
    {
        std::iter::from_fn(|| {
            let transaction = iterator.next(())?;
            iterator.mark_invalid(transaction.sender(), transaction.nonce());
            Some(*transaction.hash())
        })
        .collect()
    }

    #[test]
    fn test_simple_case() {
        let pool =
            pending_pool(&[transaction(0, 0, 1), transaction(1, 0, 1), transaction(2, 0, 1)]);

        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());
        // ### First flashblock
        iterator.refresh_iterator(parkable(&pool));
        // Accept first tx
        let tx1 = iterator.next(()).unwrap();
        iterator.mark_current_committed();
        // Invalidate second tx
        let tx2 = iterator.next(()).unwrap();
        iterator.mark_invalid(tx2.sender(), tx2.nonce());
        // Accept third tx
        let tx3 = iterator.next(()).unwrap();
        iterator.mark_current_committed();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_committed(&[*tx1.hash(), *tx3.hash()]);

        // ### Second flashblock
        // It should not return txs 1 and 3, but should return 2
        iterator.refresh_iterator(parkable(&pool));
        let tx2 = iterator.next(()).unwrap();
        iterator.mark_current_committed();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_committed(&[*tx2.hash()]);

        // ### Third flashblock
        iterator.refresh_iterator(parkable(&pool));
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
    }

    #[test]
    fn hashes_marked_committed_are_skipped_after_refresh() {
        let committed = transaction(0, 0, 2);
        let uncommitted = transaction(1, 0, 1);
        let committed_hash = *committed.hash();
        let uncommitted_hash = *uncommitted.hash();
        let pool = pending_pool(&[committed, uncommitted]);
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        iterator.mark_committed(&[committed_hash]);
        iterator.refresh_iterator(parkable(&pool));

        assert_eq!(drain_without_including(&mut iterator), vec![uncommitted_hash]);
    }

    /// Rejected transactions are skipped across flashblock boundaries within the same block.
    #[test]
    fn test_rejected_txs_persist_across_refresh() {
        let tx_2 = transaction(1, 0, 1);
        let tx_2_hash = *tx_2.hash();
        let pool = pending_pool(&[transaction(0, 0, 1), tx_2, transaction(2, 0, 1)]);

        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        // FB1: none of the transactions are included, and the second is rejected permanently
        assert_eq!(drain_without_including(&mut iterator).len(), 3);
        iterator.mark_rejected(&[tx_2_hash]);

        // FB2: refresh iterator — tx2 should still be skipped
        iterator.refresh_iterator(parkable(&pool));
        let seen_hashes = drain_without_including(&mut iterator);
        assert!(!seen_hashes.contains(&tx_2_hash), "rejected tx should not reappear after refresh");
        assert_eq!(seen_hashes.len(), 2, "only non-rejected txs should appear");
    }

    /// Rejected transactions in the shared cache are skipped by a new iterator instance
    /// (simulating cross-block persistence).
    #[test]
    fn test_rejection_cache_persists_across_blocks() {
        let tx_2 = transaction(1, 0, 1);
        let tx_2_hash = *tx_2.hash();
        let pool = pending_pool(&[transaction(0, 0, 1), tx_2]);

        let cache = test_rejection_cache();

        // Block 1: reject tx_2
        let mut iter1 = BestFlashblocksTxs::new(parkable(&pool), cache.clone());
        assert_eq!(drain_without_including(&mut iter1).len(), 2);
        iter1.mark_rejected(&[tx_2_hash]);

        // Block 2: new iterator, same cache — tx_2 should be skipped
        let mut iter2 = BestFlashblocksTxs::new(parkable(&pool), cache);
        let seen_hashes = drain_without_including(&mut iter2);
        assert!(
            !seen_hashes.contains(&tx_2_hash),
            "tx rejected in block 1 should be skipped in block 2"
        );
        assert_eq!(seen_hashes.len(), 1, "only non-rejected tx should appear");
    }

    #[test]
    fn rejection_cache_hit_excludes_nonce_descendants() {
        let rejected = transaction(0, 0, 3);
        let descendant = transaction(0, 1, 2);
        let other = transaction(1, 0, 1);
        let rejected_hash = *rejected.hash();
        let other_hash = *other.hash();
        let pool = pending_pool(&[rejected, descendant, other]);
        let cache = test_rejection_cache();
        cache.insert(rejected_hash);
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), cache);

        let yielded_hashes = drain_without_including(&mut iterator);

        assert_eq!(yielded_hashes, vec![other_hash]);
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
        let tx_a = transaction(0, 0, 1_000_000_000); // 1 gwei - LOW
        let tx_b = transaction(0, 1, 100_000_000_000); // 100 gwei - HIGH (depends on TX_A)
        let tx_c = transaction(1, 0, 10_000_000_000); // 10 gwei - MEDIUM
        let mut pool = pending_pool(&[Arc::clone(&tx_a)]);

        // === FLASHBLOCK 1 ===
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache());

        // Simulate: Flashblock 1 starts building
        // Start consuming txns from the txpool
        let first = iterator.next(()).unwrap();
        assert_eq!(*first.hash(), *tx_a.hash(), "First should be TX_A (1 gwei)");
        iterator.mark_current_committed();

        // TX_B and TX_C arrive late, but we have already yielded lower-priority transactions
        // from the iterator, so these do not immediately get added to the best txns
        pool.add_transaction(Arc::clone(&tx_b), 0);
        pool.add_transaction(Arc::clone(&tx_c), 0);
        assert!(iterator.next(()).is_none());

        // Simulate: flashblock 1 is complete after TX_A was executed
        iterator.mark_committed(&[*tx_a.hash()]);
        // Simulate pool.prune_transactions by recreating the pool without TX_A
        let pool = pending_pool(&[Arc::clone(&tx_b), Arc::clone(&tx_c)]);

        // === FLASHBLOCK 2 ===
        // We refresh the iterator with the latest best transactions
        iterator.refresh_iterator(parkable(&pool));

        // TX_A has already been executed, so TX_B (100 gwei) is the best txn and TX_C
        // (10 gwei) the second best
        assert_eq!(drain_without_including(&mut iterator), vec![*tx_b.hash(), *tx_c.hash()]);
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
            test_utils::{MockTransaction, testing_pool},
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

    /// A rejected transaction becomes eligible again after the cache TTL expires.
    #[test]
    fn test_rejected_tx_eligible_after_ttl_expiry() {
        let tx_2 = transaction(1, 0, 1);
        let tx_2_hash = *tx_2.hash();
        let pool = pending_pool(&[transaction(0, 0, 1), tx_2]);

        // TTL is short, 1ms
        let cache = RejectionCache::new(1000, Duration::from_millis(1));

        // Reject tx_2
        let mut iter1 = BestFlashblocksTxs::new(parkable(&pool), cache.clone());
        assert_eq!(drain_without_including(&mut iter1).len(), 2);
        iter1.mark_rejected(&[tx_2_hash]);

        // Wait for TTL to expire and flush pending evictions
        std::thread::sleep(Duration::from_millis(50));
        cache.run_pending_tasks();

        // New iterator — tx_2 should be back
        let mut iter2 = BestFlashblocksTxs::new(parkable(&pool), cache);
        let seen_hashes = drain_without_including(&mut iter2);
        assert!(seen_hashes.contains(&tx_2_hash), "tx should be eligible again after TTL expiry");
        assert_eq!(seen_hashes.len(), 2, "both txs should appear");
    }

    fn balance_at_least(address: Address, value: u64) -> ValidityPredicate {
        ValidityPredicate::Balance {
            address,
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(value),
        }
    }

    fn balance_change(address: Address, old: u64, new: u64) -> EvmState {
        let mut account = Account::default();
        account.info.balance = U256::from(new);
        account.original_info_mut().balance = U256::from(old);
        EvmState::from_iter([(address, account)])
    }

    fn resting_iterator(
        pool: &PendingPool<Ordering>,
    ) -> BestFlashblocksTxs<BasePooledTransaction, Parkable> {
        BestFlashblocksTxs::new(parkable(pool), test_rejection_cache())
            .with_resting_predicate_mode(RestingPredicateMode::Enforce)
    }

    /// Plays the build loop's part for a candidate whose predicate is unsatisfied.
    fn park_unsatisfied(
        iterator: &mut BestFlashblocksTxs<BasePooledTransaction, Parkable>,
        transaction: &BasePooledTransaction,
    ) {
        iterator.park_current();
        iterator.rest(*transaction.hash(), &transaction.validity_predicates()[0]);
    }

    #[test]
    fn resting_transaction_is_held_back_until_its_state_changes() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let unrelated = transaction(1, 0, 5);
        let trigger = transaction(2, 0, 4);
        let low = transaction(3, 0, 1);
        let pool = pending_pool(&[
            Arc::clone(&resting),
            Arc::clone(&unrelated),
            Arc::clone(&trigger),
            Arc::clone(&low),
        ]);
        let mut iterator = resting_iterator(&pool);

        // Flashblock 1: the build loop finds the predicate unsatisfied.
        let first = iterator.next(()).unwrap();
        assert_eq!(*first.hash(), *resting.hash());
        park_unsatisfied(&mut iterator, &first);

        // Flashblock 2: the resting transaction is not yielded, and a commit to unrelated state
        // does not release it.
        iterator.refresh_iterator(parkable(&pool));
        assert_eq!(*iterator.next(()).unwrap().hash(), *unrelated.hash());
        iterator.record_committed_state(&balance_change(UNRELATED, 0, 1));
        iterator.mark_current_committed();
        assert_eq!(iterator.take_resting_stats().parked, 1);

        // A commit to the watched balance releases it ahead of lower-priority candidates.
        assert_eq!(*iterator.next(()).unwrap().hash(), *trigger.hash());
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *low.hash());
    }

    #[test]
    fn nonce_descendant_waits_for_resting_parent() {
        let parent = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let child = transaction(0, 1, 100);
        let other = transaction(1, 0, 1);
        let pool = pending_pool(&[Arc::clone(&parent), Arc::clone(&child), Arc::clone(&other)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        iterator.refresh_iterator(parkable(&pool));
        assert_eq!(*iterator.next(()).unwrap().hash(), *other.hash());
        iterator.mark_current_committed();
        assert!(iterator.next(()).is_none());

        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert_eq!(*iterator.next(()).unwrap().hash(), *parent.hash());
        iterator.mark_current_committed();
        assert_eq!(*iterator.next(()).unwrap().hash(), *child.hash());
    }

    /// A transaction parked by the build loop in the current flashblock is woken by the build
    /// loop's own predicate index, which re-evaluates it first, so the iterator does not promote
    /// it. It no longer rests, so the next flashblock yields it for evaluation.
    #[test]
    fn transaction_parked_by_build_loop_is_not_promoted_by_a_wake() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 2)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert!(iterator.next(()).is_none());

        iterator.refresh_iterator(parkable(&pool));
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }

    #[test]
    fn latest_rested_predicate_replaces_the_previous_one() {
        let resting = validity_transaction(
            0,
            0,
            10,
            vec![balance_at_least(WATCHED, 1), balance_at_least(UNRELATED, 1)],
        );
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);
        iterator.rest(*resting.hash(), &first.validity_predicates()[1]);

        iterator.refresh_iterator(parkable(&pool));
        assert!(iterator.next(()).is_none());
        iterator.record_committed_state(&balance_change(WATCHED, 0, 1));
        assert!(iterator.next(()).is_none());
        iterator.record_committed_state(&balance_change(UNRELATED, 0, 1));
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }

    #[test]
    fn readded_hash_without_the_rested_predicate_is_yielded() {
        let original = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let readded = validity_transaction(0, 0, 10, vec![balance_at_least(UNRELATED, 1)]);
        assert_eq!(*original.hash(), *readded.hash());
        let mut iterator = resting_iterator(&pending_pool(&[original]));

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        iterator.refresh_iterator(parkable(&pending_pool(&[Arc::clone(&readded)])));
        assert_eq!(*iterator.next(()).unwrap().hash(), *readded.hash());
    }

    #[test]
    fn flashblock_index_predicate_does_not_rest() {
        let resting = validity_transaction(
            0,
            0,
            10,
            vec![ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(3),
            }],
        );
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = resting_iterator(&pool);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        iterator.refresh_iterator(parkable(&pool));
        assert_eq!(*iterator.next(()).unwrap().hash(), *resting.hash());
    }

    #[test]
    fn shadow_mode_tracks_resting_transactions_without_holding_them_back() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let pool = pending_pool(&[Arc::clone(&resting)]);
        let mut iterator = BestFlashblocksTxs::new(parkable(&pool), test_rejection_cache())
            .with_resting_predicate_mode(RestingPredicateMode::Shadow);

        let first = iterator.next(()).unwrap();
        park_unsatisfied(&mut iterator, &first);

        iterator.refresh_iterator(parkable(&pool));
        let yielded = iterator.next(()).unwrap();
        assert_eq!(*yielded.hash(), *resting.hash());
        assert!(iterator.is_resting(*resting.hash(), yielded.validity_conditions()));
    }

    #[test]
    fn off_mode_does_not_track_resting_transactions() {
        let resting = validity_transaction(0, 0, 10, vec![balance_at_least(WATCHED, 1)]);
        let predicates = resting.transaction.validity_predicates();
        let mut iterator = BestFlashblocksTxs::new(
            parkable(&pending_pool(&[Arc::clone(&resting)])),
            test_rejection_cache(),
        );

        iterator.rest(*resting.hash(), &predicates[0]);

        assert!(!iterator.is_resting(*resting.hash(), resting.transaction.validity_conditions()));
    }
}
