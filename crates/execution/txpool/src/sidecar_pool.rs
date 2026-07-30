//! SPIKE ONLY: generic pluggable sidecar sub-pool seam.

use std::sync::Arc;

use alloy_primitives::{Address, TxHash};
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolResult, TransactionOrigin, ValidPoolTransaction,
};

use crate::BasePooledTx;

/// Why a sidecar pool is being asked to drop transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalReason {
    /// The transactions became canonical.
    Mined,
    /// The transactions are being discarded.
    Discarded,
}

/// Result of a sidecar insertion.
#[derive(Debug)]
pub struct SidecarInsert<T: BasePooledTx> {
    /// reth-shaped outcome (hash + pending/queued state).
    pub outcome: AddedTransactionOutcome,
    /// Transaction evicted by a replacement, if any.
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
    /// Transactions promoted from queued to pending by this insertion.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// The inserted transaction as stored.
    pub inserted: Arc<ValidPoolTransaction<T>>,
}

/// An out-of-tree transaction sub-pool that participates in the base pool.
pub trait SidecarPool<T: BasePooledTx>: Send + Sync + std::fmt::Debug + 'static {
    /// Stable identifier used for metrics labels.
    fn name(&self) -> &'static str;

    /// Whether this pool owns `transaction`.
    fn claims(&self, transaction: &T) -> bool;

    /// Inserts an already-validated transaction. Takes ownership so the pool can
    /// mint its own `TransactionId`.
    fn insert_validated(
        &self,
        origin: TransactionOrigin,
        transaction: ValidPoolTransaction<T>,
        state_nonce: u64,
    ) -> PoolResult<SidecarInsert<T>>;

    /// Owning snapshot iterator; must not hold an internal lock across yields.
    fn best_transactions(
        &self,
        base_fee: u64,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>;

    fn contains(&self, hash: &TxHash) -> bool;
    fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>>;
    fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;
    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;
    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;
    fn pending_and_queued_txn_count(&self) -> (usize, usize);
    fn all_hashes(&self) -> Vec<TxHash>;

    fn remove_transactions(
        &self,
        hashes: &[TxHash],
        reason: RemovalReason,
    ) -> Vec<Arc<ValidPoolTransaction<T>>>;

    fn remove_transactions_and_descendants(
        &self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_transactions(hashes, RemovalReason::Discarded)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>>;

    /// Per-block maintenance. Returns `(mined, discarded)`.
    fn on_canonical_state_change(
        &self,
        _mined: &[TxHash],
        _block_timestamp_secs: u64,
    ) -> (Vec<Arc<ValidPoolTransaction<T>>>, Vec<Arc<ValidPoolTransaction<T>>>) {
        (Vec::new(), Vec::new())
    }

    fn transactions_by_sender(&self, _sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        Vec::new()
    }
    fn pending_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        Vec::new()
    }
    fn queued_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        Vec::new()
    }
    fn unique_senders(&self) -> Vec<Address> {
        Vec::new()
    }
}
