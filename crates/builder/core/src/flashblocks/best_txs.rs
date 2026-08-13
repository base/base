//! An adapter over `BestPayloadTransactions`

use std::{collections::HashSet, sync::Arc};

use alloy_primitives::{Address, TxHash, U256};
use base_execution_txpool::BasePooledTx;
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::ValidPoolTransaction;

use crate::{BuilderMetrics, RejectionCache};

/// A nonce lane whose transactions must be ordered together during a flashblock build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlashblocksNonceLane {
    /// The regular account-nonce lane for a sender.
    Account(Address),
    /// A finite EIP-8130 nonce channel for a sender.
    Channel {
        /// Sender that owns the channel.
        sender: Address,
        /// EIP-8130 nonce key that identifies the channel.
        nonce_key: U256,
    },
    /// An independent nonce-free EIP-8130 transaction.
    NonceFree(TxHash),
}

impl FlashblocksNonceLane {
    /// Derives the nonce lane used to schedule `transaction`.
    #[must_use]
    pub fn from_transaction<T: BasePooledTx>(transaction: &T) -> Self {
        transaction.eip8130_nonce_channel_key().map_or_else(
            || {
                if transaction.eip8130_replay_id().is_some() {
                    Self::NonceFree(*transaction.hash())
                } else {
                    Self::Account(transaction.sender())
                }
            },
            |nonce_key| Self::Channel { sender: transaction.sender(), nonce_key },
        )
    }
}

/// An adapter over `BestPayloadTransactions` that allows to skip transactions that were already
/// committed to the state. It also allows to refresh inner iterator on each flashblock building, to
/// update priority boundaries.
pub struct BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    inner: reth_payload_util::BestPayloadTransactions<T, I>,
    // Transactions that were already committed to the state. Using them again would cause NonceTooLow
    // so we skip them
    committed_transactions: HashSet<TxHash>,
    // Nonce lanes blocked by a state-dependent build-time check. They are reconsidered after the
    // next iterator refresh.
    blocked_lanes: HashSet<FlashblocksNonceLane>,
    // Lane of the most recently yielded transaction. This lets build-time validation block the
    // exact EIP-8130 lane without reducing it to a sender address.
    current_lane: Option<FlashblocksNonceLane>,
    // Shared cross-block rejection cache (survives across blocks, TTL-bounded)
    rejection_cache: RejectionCache,
}

impl<T, I> std::fmt::Debug for BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BestFlashblocksTxs")
            .field("committed_transactions", &self.committed_transactions)
            .field("blocked_lanes", &self.blocked_lanes)
            .field("rejection_cache_size", &self.rejection_cache.entry_count())
            .finish_non_exhaustive()
    }
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: BasePooledTx,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    /// Creates a new [`BestFlashblocksTxs`] wrapping the given payload transaction iterator.
    pub fn new(
        inner: reth_payload_util::BestPayloadTransactions<T, I>,
        rejection_cache: RejectionCache,
    ) -> Self {
        Self {
            inner,
            committed_transactions: Default::default(),
            blocked_lanes: Default::default(),
            current_lane: None,
            rejection_cache,
        }
    }

    /// Replaces current iterator with new one. We use it on new flashblock building, to refresh
    /// priority boundaries
    pub fn refresh_iterator(&mut self, inner: reth_payload_util::BestPayloadTransactions<T, I>) {
        self.inner = inner;
        self.blocked_lanes.clear();
        self.current_lane = None;
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
    T: BasePooledTx,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    type Transaction = T;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        loop {
            let tx = self.inner.next(ctx)?;
            let hash = *tx.hash();

            if self.committed_transactions.contains(&hash) {
                continue;
            }

            let lane = FlashblocksNonceLane::from_transaction(&tx);
            if self.blocked_lanes.contains(&lane) {
                continue;
            }

            if self.rejection_cache.contains_key(&hash) {
                BuilderMetrics::rejection_cache_hits().increment(1);
                continue;
            }

            self.current_lane = Some(lane);
            return Some(tx);
        }
    }

    /// Blocks the current Base nonce lane for the remainder of this flashblock.
    ///
    /// The wrapped Reth adapter invalidates all transactions from a sender. This adapter retains
    /// the EIP-8130 nonce-key identity instead, and clears the lane on the next flashblock refresh.
    fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {
        debug_assert!(
            self.current_lane.is_some(),
            "mark_invalid must follow a transaction returned by next"
        );
        if let Some(lane) = self.current_lane.take() {
            self.blocked_lanes.insert(lane);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use alloy_consensus::{SignableTransaction, Transaction, TxEip1559, transaction::Recovered};
    use alloy_eips::{eip1559::MIN_PROTOCOL_BASE_FEE, eip2718::Encodable2718};
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BaseTxEnvelope, Eip8130Signed,
        TxEip8130,
    };
    use base_execution_txpool::BasePooledTransaction;
    use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
    use reth_primitives_traits::SignerRecoverable;
    use reth_transaction_pool::{
        CoinbaseTipOrdering, PoolTransaction, TransactionOrigin, ValidPoolTransaction,
        identifier::TransactionId, pool::PendingPool, test_utils::MockTransaction,
    };

    use crate::{
        RejectionCache,
        flashblocks::best_txs::{BestFlashblocksTxs, FlashblocksNonceLane},
    };

    fn test_rejection_cache() -> RejectionCache {
        RejectionCache::new(1000, Duration::from_secs(60))
    }

    fn channelized_transaction(
        signer: &PrivateKeySigner,
        nonce_key: U256,
    ) -> BasePooledTransaction {
        let transaction = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&transaction.sender_signature_hash()).unwrap();
        let signed = Eip8130Signed::new(
            transaction,
            Bytes::from(signature.as_bytes().to_vec()),
            Bytes::new(),
        );
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn regular_transaction(
        signer: &PrivateKeySigner,
        nonce: u64,
        priority_fee: u128,
        max_fee: u128,
    ) -> BasePooledTransaction {
        let transaction = TxEip1559 {
            chain_id: ChainConfig::mainnet().chain_id,
            nonce,
            gas_limit: 50_000,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&transaction.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(transaction.into_signed(signature));
        let encoded_length = envelope.encode_2718_len();
        let recovered = envelope.try_into_recovered().unwrap();
        BasePooledTransaction::new(recovered, encoded_length)
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
        sender_id: u64,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(sender_id.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    #[test]
    fn test_simple_case() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<BasePooledTransaction>::default());

        // Add 3 regular transaction
        let tx_1 = regular_transaction(&PrivateKeySigner::random(), 0, 3, 1_000);
        let tx_2 = regular_transaction(&PrivateKeySigner::random(), 0, 2, 1_000);
        let tx_3 = regular_transaction(&PrivateKeySigner::random(), 0, 1, 1_000);
        pool.add_transaction(valid_pool_transaction(tx_1, 0), 0);
        pool.add_transaction(valid_pool_transaction(tx_2, 1), 0);
        pool.add_transaction(valid_pool_transaction(tx_3, 2), 0);

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
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<BasePooledTransaction>::default());
        let signer_a = PrivateKeySigner::random();
        let signer_b = PrivateKeySigner::random();
        let sender_a = signer_a.address();
        let sender_b = signer_b.address();

        let tx_a = regular_transaction(&signer_a, 0, 1_000_000_000, 100_000_000_000);
        let tx_b = regular_transaction(&signer_a, 1, 100_000_000_000, 200_000_000_000);
        let tx_c = regular_transaction(&signer_b, 0, 10_000_000_000, 100_000_000_000);

        pool.add_transaction(valid_pool_transaction(tx_a.clone(), 0), 0);

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
        pool.add_transaction(valid_pool_transaction(tx_b.clone(), 0), 0);
        pool.add_transaction(valid_pool_transaction(tx_c.clone(), 1), 0);
        assert!(iterator.next(()).is_none());

        // Simulate: flashblock 1 is complete after TX_A was executed
        iterator.mark_committed(&[*tx_a.hash()]);
        // Simulate pool.prune_transactions by recreating the pool without TX_A
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<BasePooledTransaction>::default());
        pool.add_transaction(valid_pool_transaction(tx_b, 0), 0);
        pool.add_transaction(valid_pool_transaction(tx_c, 1), 0);

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
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<BasePooledTransaction>::default());

        let tx_1 = regular_transaction(&PrivateKeySigner::random(), 0, 3, 1_000);
        let tx_2 = regular_transaction(&PrivateKeySigner::random(), 0, 2, 1_000);
        let tx_3 = regular_transaction(&PrivateKeySigner::random(), 0, 1, 1_000);
        let tx_2_hash = *tx_2.hash();
        pool.add_transaction(valid_pool_transaction(tx_1, 0), 0);
        pool.add_transaction(valid_pool_transaction(tx_2, 1), 0);
        pool.add_transaction(valid_pool_transaction(tx_3, 2), 0);

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

    #[test]
    fn invalidating_one_eip8130_lane_keeps_other_lanes_eligible() {
        let signer = PrivateKeySigner::random();
        let first_transaction =
            valid_pool_transaction(channelized_transaction(&signer, U256::from(1)), 0);
        let second_transaction =
            valid_pool_transaction(channelized_transaction(&signer, U256::from(2)), 0);
        let first_hash = *first_transaction.hash();

        let mut iterator = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(
                vec![Arc::clone(&first_transaction), Arc::clone(&second_transaction)].into_iter(),
            ),
            test_rejection_cache(),
        );

        let first = iterator.next(()).unwrap();
        let first_lane = FlashblocksNonceLane::from_transaction(&first);
        iterator.mark_invalid(first.sender(), first.nonce());

        let second = iterator.next(()).expect("other EIP-8130 lane remains eligible");
        assert_eq!(first.sender(), second.sender());
        assert_ne!(FlashblocksNonceLane::from_transaction(&second), first_lane);
        assert!(iterator.next(()).is_none());

        iterator.refresh_iterator(BestPayloadTransactions::new(
            vec![first_transaction, second_transaction].into_iter(),
        ));
        assert_eq!(*iterator.next(()).unwrap().hash(), first_hash);
    }

    #[test]
    fn invalidating_account_lane_keeps_eip8130_channel_eligible() {
        let signer = PrivateKeySigner::random();
        let account_transaction =
            valid_pool_transaction(regular_transaction(&signer, 0, 2, 1_000), 0);
        let channel_transaction =
            valid_pool_transaction(channelized_transaction(&signer, U256::from(1)), 0);
        let channel_hash = *channel_transaction.hash();

        let mut iterator = BestFlashblocksTxs::new(
            BestPayloadTransactions::new(
                vec![Arc::clone(&account_transaction), channel_transaction].into_iter(),
            ),
            test_rejection_cache(),
        );

        let account = iterator.next(()).unwrap();
        assert_eq!(*account.hash(), *account_transaction.hash());
        iterator.mark_invalid(account.sender(), account.nonce());

        assert_eq!(*iterator.next(()).unwrap().hash(), channel_hash);
    }

    /// Rejected transactions in the shared cache are skipped by a new iterator instance
    /// (simulating cross-block persistence).
    #[test]
    fn test_rejection_cache_persists_across_blocks() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<BasePooledTransaction>::default());

        let tx_1 = regular_transaction(&PrivateKeySigner::random(), 0, 2, 1_000);
        let tx_2 = regular_transaction(&PrivateKeySigner::random(), 0, 1, 1_000);
        let tx_2_hash = *tx_2.hash();
        pool.add_transaction(valid_pool_transaction(tx_1, 0), 0);
        pool.add_transaction(valid_pool_transaction(tx_2, 1), 0);

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

    /// A rejected transaction becomes eligible again after the cache TTL expires.
    #[test]
    fn test_rejected_tx_eligible_after_ttl_expiry() {
        let mut pool = PendingPool::new(CoinbaseTipOrdering::<BasePooledTransaction>::default());

        let tx_1 = regular_transaction(&PrivateKeySigner::random(), 0, 2, 1_000);
        let tx_2 = regular_transaction(&PrivateKeySigner::random(), 0, 1, 1_000);
        let tx_2_hash = *tx_2.hash();
        pool.add_transaction(valid_pool_transaction(tx_1, 0), 0);
        pool.add_transaction(valid_pool_transaction(tx_2, 1), 0);

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
