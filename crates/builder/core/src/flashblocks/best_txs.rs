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
    commited_transactions: HashSet<TxHash>,
}

impl<T, I> std::fmt::Debug for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BestFlashblocksTxs")
            .field("commited_transactions", &self.commited_transactions)
            .finish_non_exhaustive()
    }
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: Iterator<Item = Arc<ValidPoolTransaction<T>>>,
{
    pub fn new(inner: reth_payload_util::BestPayloadTransactions<T, I>) -> Self {
        Self { inner, commited_transactions: Default::default() }
    }

    /// Replaces current iterator with new one. We use it on new flashblock building, to refresh
    /// priority boundaries
    pub fn refresh_iterator(
        &mut self,
        inner: reth_payload_util::BestPayloadTransactions<T, I>,
    ) {
        self.inner = inner;
    }

    /// Remove transaction from next iteration and it is already in the state
    pub fn mark_commited(&mut self, txs: Vec<TxHash>) {
        self.commited_transactions.extend(txs);
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
            if self.commited_transactions.contains(tx.hash()) {
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
        iterator.mark_commited(vec![*tx1.hash(), *tx3.hash()]);

        // ### Second flashblock
        // It should not return txs 1 and 3, but should return 2
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));
        let tx2 = iterator.next(()).unwrap();
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
        // Mark transaction as committed
        iterator.mark_commited(vec![*tx2.hash()]);

        // ### Third flashblock
        iterator.refresh_iterator(BestPayloadTransactions::new(pool.best()));
        // Check that it's empty
        assert!(iterator.next(()).is_none(), "Iterator should be empty");
    }
}
