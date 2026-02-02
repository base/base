//! An adapter over `BestPayloadTransactions`

use std::{collections::HashSet, sync::Arc};

use alloy_primitives::{Address, TxHash};
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};

/// An adapter over `BestPayloadTransactions` that allows to skip transactions that were already
/// committed to the state. It also allows to refresh inner iterator on each flashblock building, to
/// update priority boundaries.
pub struct BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    inner: reth_payload_util::BestPayloadTransactions<T, I>,
    // Transactions that were already committed to the state. Using them again would cause NonceTooLow
    // so we skip them
    committed_transactions: HashSet<TxHash>,
}

impl<T, I> std::fmt::Debug for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BestFlashblocksTxs")
            .field("committed_transactions", &self.committed_transactions)
            .finish_non_exhaustive()
    }
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    pub fn new(inner: reth_payload_util::BestPayloadTransactions<T, I>) -> Self {
        Self { inner, committed_transactions: Default::default() }
    }

    /// Replaces current iterator with new one. We use it on new flashblock building, to refresh
    /// priority boundaries
    pub fn refresh_iterator(&mut self, inner: reth_payload_util::BestPayloadTransactions<T, I>) {
        self.inner = inner;
    }

    /// Remove transaction from next iteration since it is already in the state
    pub fn mark_committed(&mut self, txs: &[TxHash]) {
        self.committed_transactions.extend(txs);
    }
}

impl<T, I> PayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    type Transaction = T;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        loop {
            let tx = self.inner.next(ctx)?;
            // Skip transaction we already included
            if self.committed_transactions.contains(tx.hash()) {
                continue;
            }

            return Some(tx);
        }
    }

    /// Proxy to inner iterator
    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Transaction;
    use alloy_eips::eip1559::MIN_PROTOCOL_BASE_FEE;
    use reth_payload_util::{BestPayloadTransactions, PayloadTransactions};
    use reth_transaction_pool::{
        CoinbaseTipOrdering, PoolTransaction,
        pool::PendingPool,
        test_utils::{MockTransaction, MockTransactionFactory},
    };

    use crate::flashblocks::best_txs::BestFlashblocksTxs;

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
        let mut iterator = BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()));
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

    /// This test demonstrates the nonce-chain gating issue across flashblock boundaries.
    ///
    /// Scenario (based on real Base Mainnet block 41628995):
    /// - Sender A has TX_A (nonce 0, LOW tip) and TX_B (nonce 1, HIGH tip) in the pool
    /// - Sender B has TX_C (MEDIUM tip)
    /// TX_A is in the mempool, TX_B and TX_C arrive later after the first flashblock has
    /// started building already
    /// - In flashblock 1, TX_A gets consumed (TX_B unlocks after TX_A)
    /// - Only TX_A is marked as committed (simulating flashblock timer expiring)
    /// - In flashblock 2, TX_B (HIGH tip) should come before TX_C (MEDIUM tip)
    ///
    /// Expected: TX_B (100 gwei) before TX_C (10 gwei) in flashblock 2
    /// Actual: TX_C comes first because pool still thinks TX_A is pending
    ///
    /// This test is intentionally written to FAIL to prove the bug exists.
    #[test]
    fn test_nonce_chain_gating_bug_across_flashblocks() {
        use alloy_primitives::Address;

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
        let mut iterator = BestFlashblocksTxs::new(BestPayloadTransactions::new(pool.best()));

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
        iterator.mark_commited(vec![*tx_a.hash()]);

        // === FLASHBLOCK 2 ===
        // We refresh the iterator with the latest best transactions
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));

        // Now, theoretically, TX_A has already been executed, so
        // TX_B should be the best txn and TX_C the second best
        // Expected: TX_B (100 gwei) first, TX_C (10 gwei) second
        // Actual bug: TX_C first, then TX_A skipped, then TX_B
        let fb2_first = iterator.next(()).unwrap();
        let fb2_second = iterator.next(()).unwrap();

        // If this test passes, the bug has been proven
        assert_eq!(fb2_first.sender(), sender_b);
        assert_eq!(fb2_second.sender(), sender_a);
        assert!(
            fb2_second.effective_tip_per_gas(MIN_PROTOCOL_BASE_FEE)
                > fb2_first.effective_tip_per_gas(MIN_PROTOCOL_BASE_FEE)
        );

        panic!("Critical ordering bug");

        // SOLUTION:
        // We need to loop over `committed_transactions` HashSet
        // when we refresh the iterator
        // and ensure we consume txns from the txpool that were executed in previous
        // flashblocks before we start building the new one
    }
}
