//! A generic, pluggable sidecar sub-pool seam.
//!
//! [`SidecarPool`] lets a crate outside this one contribute an extra sub-pool to
//! [`BaseTransactionPool`](crate::BaseTransactionPool), alongside the protocol pool and the 2D
//! nonce pool. The host keeps validation, listener broadcast, and the EIP-8130 admission guard;
//! the sidecar owns its storage, its identifiers, and its internal ordering.
//!
//! # Implementor contract
//!
//! - **Locking.** Every method is called with no pool lock held, and must not call back into the
//!   pool that owns it. The host's own order, where it takes more than one, is `nonce_pool` →
//!   `listeners` → `guard`.
//! - **Claiming.** [`SidecarPool::claims`] must be stable for a given transaction and must not
//!   overlap another registered sidecar; the first claimant in registration order wins.
//! - **Ordering.** [`SidecarPool::best_transactions`] must yield this pool's own best transaction
//!   first, respecting whatever internal dependencies it has. The host merges the arms by
//!   comparing their heads, so a pool orders its own transactions however it likes; it does not
//!   need to know, and cannot see, the host's ordering.
//! - **Eviction.** Nothing else drops a sidecar's transactions. A pool that never returns anything
//!   from [`SidecarPool::on_canonical_state_change`] and never prunes internally grows unbounded,
//!   and leaks an admission-guard slot per transaction.

use std::sync::Arc;

use alloy_primitives::{Address, TxHash};
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolResult, TransactionOrigin, ValidPoolTransaction,
    validate::ValidTransaction,
};

use crate::BasePooledTx;

/// Why a sidecar pool is being asked to drop transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalReason {
    /// The transactions became canonical.
    ///
    /// A pool sequencing transactions per sender should advance its cursor for these, and only
    /// these — a discard leaves the sequence where it was.
    Mined,
    /// The transactions are being discarded without being mined.
    Discarded,
    /// As [`Self::Discarded`], and also drop anything their removal leaves un-includable.
    ///
    /// A pool whose transactions depend on one another — a per-sender nonce sequence, say — must
    /// also evict the dependents. A pool with no such dependencies treats this exactly as
    /// [`Self::Discarded`].
    DiscardedWithDescendants,
}

/// A validated transaction being handed to a sidecar for storage.
///
/// The host does *not* build a [`ValidPoolTransaction`] first. Doing so would mean minting a
/// `TransactionId` out of a sender interner the sidecar is then told to ignore — which both
/// wasted the work and accumulated an entry per sender that nothing could ever free. The sidecar
/// owns its identifier space, so it builds the stored form itself.
#[derive(Debug)]
pub struct SidecarAdmission<T: BasePooledTx> {
    /// Where the transaction came from.
    pub origin: TransactionOrigin,
    /// The validated transaction, still carrying its blob sidecar if it had one.
    pub transaction: ValidTransaction<T>,
    /// Whether the transaction may be gossiped.
    pub propagate: bool,
    /// EIP-7702 authorities recovered during validation, if any.
    pub authorities: Option<Vec<Address>>,
    /// The sender's on-chain nonce as of validation.
    ///
    /// A pool that sequences transactions per sender needs this to anchor its cursor, and to
    /// re-anchor it after a reorg moves the account backwards. Pools keyed on anything other than
    /// account nonces ignore it.
    pub state_nonce: u64,
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

/// A sidecar pool's contribution to [`TransactionPool::pool_size`].
///
/// [`TransactionPool::pool_size`]: reth_transaction_pool::TransactionPool::pool_size
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SidecarPoolSize {
    /// Number of transactions eligible for inclusion.
    pub pending: usize,
    /// Encoded byte total of the pending transactions.
    pub pending_size: usize,
    /// Number of transactions held back.
    pub queued: usize,
    /// Encoded byte total of the queued transactions.
    pub queued_size: usize,
}

/// The `(mined, discarded)` split a sidecar pool reports back from per-block maintenance.
pub type CanonicalStateOutcome<T> =
    (Vec<Arc<ValidPoolTransaction<T>>>, Vec<Arc<ValidPoolTransaction<T>>>);

/// An out-of-tree transaction sub-pool that participates in the base pool.
///
/// Deliberately small. Everything the host can compute from these methods — membership,
/// enumeration, per-sender views, counts, removal by sender — it computes, rather than asking
/// each implementor to reimplement it.
pub trait SidecarPool<T: BasePooledTx>: Send + Sync + std::fmt::Debug + 'static {
    /// Stable identifier, used for metrics labels and diagnostics.
    fn name(&self) -> &'static str;

    /// Whether this pool owns `transaction`.
    ///
    /// Decides routing at admission and is consulted again to attribute a transaction back to
    /// this pool, so it must not depend on whether the transaction is currently stored.
    fn claims(&self, transaction: &T) -> bool;

    /// Stores a validated transaction, minting whatever identifier this pool's storage needs.
    fn insert(&self, admission: SidecarAdmission<T>) -> PoolResult<SidecarInsert<T>>;

    /// An owning snapshot of this pool's transactions in its own preferred order, best first.
    ///
    /// Must not hold an internal lock across yields. See the ordering note in the module docs:
    /// the host merges arms by comparing heads, so this pool's order is respected as-is.
    fn best_transactions(
        &self,
        base_fee: u64,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>;

    /// The transaction with `hash`, if this pool holds it.
    fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>>;

    /// The transactions currently eligible for inclusion.
    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;

    /// The transactions held back until some precondition is met.
    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>;

    /// Drops the named transactions, returning those actually removed.
    ///
    /// `reason` distinguishes inclusion from discard: an implementor tracking a per-sender
    /// sequence should only advance it for [`RemovalReason::Mined`].
    fn remove_transactions(
        &self,
        hashes: &[TxHash],
        reason: RemovalReason,
    ) -> Vec<Arc<ValidPoolTransaction<T>>>;

    /// Counts and byte totals, for the pool-status RPCs and metrics.
    ///
    /// The default materializes both transaction vectors just to measure them, and runs on a path
    /// that serves metrics and RPC. Override it if this pool can answer in `O(1)`.
    fn size(&self) -> SidecarPoolSize {
        let pending = self.pending_transactions();
        let queued = self.queued_transactions();
        SidecarPoolSize {
            pending: pending.len(),
            pending_size: pending.iter().map(|tx| tx.encoded_length()).sum(),
            queued: queued.len(),
            queued_size: queued.iter().map(|tx| tx.encoded_length()).sum(),
        }
    }

    /// Per-block maintenance. Returns `(mined, discarded)`.
    ///
    /// The returned transactions are what the host reports to listeners and releases from the
    /// admission guard, so a transaction dropped internally but omitted here leaks its guard slot.
    /// Defaulting to empty is only correct for a pool that never needs to evict.
    fn on_canonical_state_change(
        &self,
        _mined: &[TxHash],
        _block_timestamp_secs: u64,
    ) -> CanonicalStateOutcome<T> {
        (Vec::new(), Vec::new())
    }
}
