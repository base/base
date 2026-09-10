//! Transaction-pool backed candidate source for engine cache prewarming.

use std::fmt::Debug;

use alloy_primitives::B256;
use base_execution_engine_driver::tree::{
    TxPoolPrewarmSource as PrewarmSource, TxPoolPrewarmTransaction as Transaction,
    TxPoolPrewarmTransactions as Transactions,
};
use base_execution_txpool::{BestTransactions, BestTransactionsAttributes, TransactionPool};

/// [`TransactionPool`]-backed [`PrewarmSource`].
#[derive(Debug)]
pub struct TxpoolPrewarmSource<P>(pub P);

impl<P> TxpoolPrewarmSource<P> {
    /// Creates a new txpool prewarm source.
    pub const fn new(pool: P) -> Self {
        Self(pool)
    }
}

impl<P> PrewarmSource for TxpoolPrewarmSource<P>
where
    P: TransactionPool + Clone + Send + Sync + Debug + 'static,
{
    fn best_transactions(&self, parent_hash: B256) -> Option<Transactions> {
        let block_info = self.0.block_info();
        if block_info.last_seen_block_hash != parent_hash {
            return None;
        }

        let mut best = self.0.best_transactions_with_attributes(BestTransactionsAttributes::new(
            block_info.pending_basefee,
            block_info.pending_blob_fee.map(|fee| u64::try_from(fee).unwrap_or(u64::MAX)),
        ));
        best.allow_updates_out_of_order();
        best.skip_blobs();

        Some(Box::new(best.map(|transaction| Transaction {
            hash: *transaction.hash(),
            sender: transaction.sender(),
            transaction: transaction.transaction.clone_into_consensus(),
        })))
    }
}
