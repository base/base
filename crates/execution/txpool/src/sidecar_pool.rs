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

/// The `(mined, discarded)` split a sidecar pool reports back from per-block maintenance.
pub type CanonicalStateOutcome<T> =
    (Vec<Arc<ValidPoolTransaction<T>>>, Vec<Arc<ValidPoolTransaction<T>>>);

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

    /// Whether this pool holds a transaction with `hash`.
    fn contains(&self, hash: &TxHash) -> bool;
    /// The transaction with `hash`, if this pool holds it.
    fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>>;
    /// Every transaction this pool holds, pending and queued alike.
    fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;
    /// The transactions currently eligible for inclusion.
    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;
    /// The transactions held back until some precondition is met.
    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;
    /// `(pending, queued)` counts, for the pool-status RPCs.
    fn pending_and_queued_txn_count(&self) -> (usize, usize);
    /// The hashes of every transaction this pool holds.
    fn all_hashes(&self) -> Vec<TxHash>;

    /// Drops the named transactions, returning those actually removed.
    ///
    /// `reason` distinguishes inclusion from discard: an implementor tracking a per-sender
    /// sequence should only advance it for [`RemovalReason::Mined`].
    fn remove_transactions(
        &self,
        hashes: &[TxHash],
        reason: RemovalReason,
    ) -> Vec<Arc<ValidPoolTransaction<T>>>;

    /// Drops the named transactions plus anything their removal leaves un-includable.
    ///
    /// Defaults to a plain discard, which is correct for a pool whose transactions carry no
    /// dependency on one another.
    fn remove_transactions_and_descendants(
        &self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_transactions(hashes, RemovalReason::Discarded)
    }

    /// Drops every transaction from `sender`, returning those removed.
    fn remove_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>>;

    /// Per-block maintenance. Returns `(mined, discarded)`.
    fn on_canonical_state_change(
        &self,
        _mined: &[TxHash],
        _block_timestamp_secs: u64,
    ) -> CanonicalStateOutcome<T> {
        (Vec::new(), Vec::new())
    }

    /// This pool's contribution to the account-nonce query surface for `sender`.
    ///
    /// Defaults to empty, which is the safe answer: a pool whose transactions live in a
    /// namespace disjoint from account nonces must not appear here, or `eth_getTransactionCount`
    /// will double-count. Override only if your transactions genuinely occupy the sender's
    /// account-nonce sequence.
    fn transactions_by_sender(&self, _sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        Vec::new()
    }
    /// The pending subset of [`transactions_by_sender`](Self::transactions_by_sender).
    fn pending_transactions_by_sender(
        &self,
        _sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        Vec::new()
    }
    /// The queued subset of [`transactions_by_sender`](Self::transactions_by_sender).
    fn queued_transactions_by_sender(&self, _sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        Vec::new()
    }
    /// Every sender this pool holds a transaction for. Same opt-in caveat as
    /// [`transactions_by_sender`](Self::transactions_by_sender).
    fn unique_senders(&self) -> Vec<Address> {
        Vec::new()
    }
}
