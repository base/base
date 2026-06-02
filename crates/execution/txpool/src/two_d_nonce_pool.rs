//! Minimal 2D nonce sidecar storage and iteration for channelized EIP-8130 transactions.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use alloy_primitives::{Address, B256, TxHash, U256};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolResult, PriceBumpConfig, Priority,
    TransactionOrdering, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    identifier::{SenderIdentifiers, TransactionId},
    pool::{AddedTransactionState, QueuedReason},
};

use crate::BasePooledTx;

type LaneId = (Address, U256);

#[derive(Debug)]
struct NonceLane<T: BasePooledTx> {
    next_nonce: u64,
    transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> Default for NonceLane<T> {
    fn default() -> Self {
        Self { next_nonce: 0, transactions: BTreeMap::new() }
    }
}

impl<T: BasePooledTx> NonceLane<T> {
    fn consecutive_pending_transactions(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions
            .range(self.next_nonce..)
            .enumerate()
            .take_while(|(offset, (nonce, _))| {
                self.next_nonce
                    .checked_add(*offset as u64)
                    .is_some_and(|expected| **nonce == expected)
            })
            .map(|(_, (_, transaction))| transaction)
    }

    fn consecutive_pending_len(&self) -> usize {
        self.consecutive_pending_transactions().count()
    }

    fn queued_transactions(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions
            .range(self.next_nonce..)
            .map(|(_, transaction)| transaction)
            .skip(self.consecutive_pending_len())
    }
}

/// Outcome returned after inserting into the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct InsertOutcome<T: BasePooledTx> {
    pub outcome: AddedTransactionOutcome,
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
}

/// Minimal 2D nonce sidecar for finite non-zero `nonce_key` channels and
/// nonce-free (`NONCE_KEY_MAX`) transactions.
///
/// Channelized transactions live in per-`(sender, nonce_key)` [`NonceLane`]s
/// with sequential-nonce semantics. Nonce-free transactions instead live in
/// [`Self::nonce_free_txs`], keyed by their signature-invariant replay
/// identifier (see [`crate::BasePooledTx::eip8130_nonce_free_replay_id`]); they
/// are always pending, never form lanes, and are deduplicated so that re-signed
/// `payer_auth`/`sender_auth` variants of one logical transaction collapse to a
/// single entry.
#[derive(Debug)]
pub(crate) struct TwoDNoncePool<T: BasePooledTx> {
    lanes: HashMap<LaneId, NonceLane<T>>,
    hashes: HashMap<TxHash, Arc<ValidPoolTransaction<T>>>,
    index: HashMap<TxHash, (LaneId, u64)>,
    /// Nonce-free transactions keyed by their replay identifier.
    nonce_free_txs: HashMap<B256, Arc<ValidPoolTransaction<T>>>,
    /// Reverse index from transaction hash to replay identifier for nonce-free
    /// transactions.
    nonce_free_by_hash: HashMap<TxHash, B256>,
    /// Per-sender count of nonce-free transactions, used to enforce
    /// [`Self::MAX_NONCE_FREE_TXS_PER_SENDER`].
    nonce_free_count_by_sender: HashMap<Address, usize>,
    /// Hashes of EIP-8130 transactions with a non-zero `expiry`, bucketed by
    /// expiry timestamp. Enables [`Self::evict_expired`] to find expired
    /// transactions via a range scan instead of iterating the whole sidecar on
    /// every canonical state change.
    expiry_index: BTreeMap<u64, HashSet<TxHash>>,
    senders: SenderIdentifiers,
    price_bump_config: PriceBumpConfig,
}

impl<T: BasePooledTx> TwoDNoncePool<T> {
    /// Maximum number of concurrent nonce-free EIP-8130 transactions a single
    /// sender may keep in the sidecar.
    ///
    /// Replay-id dedup only collapses re-signed variants of the *same* logical
    /// body, so each distinct nonce-free body (e.g. a different `max_fee_per_gas`
    /// or `expiry`) yields a fresh `replay_id`. Without this cap a single sender
    /// could insert an unbounded number of nonce-free transactions, exhausting
    /// sidecar memory and the eviction-scan budget. The limit mirrors the
    /// per-account slot bound used by the protocol pools.
    const MAX_NONCE_FREE_TXS_PER_SENDER: usize = 16;

    /// Creates a new 2D nonce sidecar pool.
    pub(crate) fn new(price_bump_config: PriceBumpConfig) -> Self {
        Self {
            lanes: HashMap::new(),
            hashes: HashMap::new(),
            index: HashMap::new(),
            nonce_free_txs: HashMap::new(),
            nonce_free_by_hash: HashMap::new(),
            nonce_free_count_by_sender: HashMap::new(),
            expiry_index: BTreeMap::new(),
            senders: SenderIdentifiers::default(),
            price_bump_config,
        }
    }

    /// Returns true if the sidecar already contains the hash.
    pub(crate) fn contains(&self, hash: &TxHash) -> bool {
        self.hashes.contains_key(hash)
    }

    /// Returns the number of pending and queued transactions.
    ///
    /// Nonce-free transactions are always counted as pending.
    pub(crate) fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let mut pending = self.nonce_free_txs.len();
        let mut queued = 0;
        for lane in self.lanes.values() {
            let pending_in_lane = lane.consecutive_pending_len();
            pending += pending_in_lane;
            queued += lane.transactions.len().saturating_sub(pending_in_lane);
        }
        (pending, queued)
    }

    /// Returns all pending transactions, including all nonce-free transactions.
    pub(crate) fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for transaction in lane.consecutive_pending_transactions() {
                transactions.push(Arc::clone(transaction));
            }
        }
        transactions.extend(self.nonce_free_txs.values().cloned());
        transactions
    }

    /// Returns all queued transactions.
    pub(crate) fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for lane in self.lanes.values() {
            for transaction in lane.queued_transactions() {
                transactions.push(Arc::clone(transaction));
            }
        }
        transactions
    }

    /// Returns all transactions in the sidecar.
    pub(crate) fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.hashes.values().cloned().collect()
    }

    /// Returns all transaction hashes in the sidecar.
    pub(crate) fn all_hashes(&self) -> Vec<TxHash> {
        self.hashes.keys().copied().collect()
    }

    /// Returns the transaction for the given hash.
    pub(crate) fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.hashes.get(hash).cloned()
    }

    /// Returns transactions for the given sender.
    pub(crate) fn transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.hashes.values().filter(|tx| tx.sender() == sender).cloned().collect()
    }

    /// Returns pending transactions for the given sender.
    pub(crate) fn pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions().into_iter().filter(|tx| tx.sender() == sender).collect()
    }

    /// Returns queued transactions for the given sender.
    pub(crate) fn queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.queued_transactions().into_iter().filter(|tx| tx.sender() == sender).collect()
    }

    /// Returns the highest transaction for the sender across all nonce channels.
    pub(crate) fn highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.transactions_by_sender(sender).into_iter().max_by_key(|tx| tx.nonce())
    }

    /// Returns the highest pending transaction for the sender.
    pub(crate) fn highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions_by_sender(sender).into_iter().max_by_key(|tx| tx.nonce())
    }

    /// Returns the first transaction that matches the sender and nonce sequence.
    pub(crate) fn transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.transactions_by_sender(sender).into_iter().find(|tx| tx.nonce() == nonce)
    }

    /// Returns all senders present in the sidecar.
    pub(crate) fn unique_senders(&self) -> HashSet<Address> {
        self.hashes.values().map(|tx| tx.sender()).collect()
    }

    /// Returns or creates the sender id for the given address.
    pub(crate) fn sender_id_or_create(
        &mut self,
        address: Address,
    ) -> reth_transaction_pool::identifier::SenderId {
        self.senders.sender_id_or_create(address)
    }

    /// Inserts a validated channelized or nonce-free EIP-8130 transaction.
    pub(crate) fn insert_validated(
        &mut self,
        mut transaction: ValidPoolTransaction<T>,
    ) -> PoolResult<InsertOutcome<T>> {
        let hash = *transaction.hash();
        if self.contains(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }

        let sender = transaction.sender();

        if let Some(replay_id) = transaction.transaction.eip8130_nonce_free_replay_id() {
            return self.insert_nonce_free(transaction, hash, sender, replay_id);
        }

        let nonce_key = transaction.transaction.eip8130_nonce_channel_key().ok_or_else(|| {
            PoolError::other(
                hash,
                "2D nonce pool only accepts channelized or nonce-free EIP-8130 transactions",
            )
        })?;

        let lane_id = (sender, nonce_key);
        let sender_id = self.senders.sender_id_or_create(sender);
        let nonce = transaction.nonce();
        transaction.transaction_id = TransactionId::new(sender_id, nonce);
        let transaction = Arc::new(transaction);
        let lane = self.lanes.entry(lane_id).or_default();
        let pending_len_before = lane.consecutive_pending_len();

        if nonce < lane.next_nonce {
            return Err(PoolError::new(
                hash,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::NonceNotConsistent {
                        tx: nonce,
                        state: lane.next_nonce,
                    },
                )),
            ));
        }

        let replaced: Option<Arc<ValidPoolTransaction<T>>> =
            if let Some(existing) = lane.transactions.get(&nonce) {
                if existing.is_underpriced(&transaction, &self.price_bump_config) {
                    return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
                }
                Some(Arc::clone(existing))
            } else {
                None
            };

        lane.transactions.insert(nonce, Arc::clone(&transaction));
        self.hashes.insert(hash, Arc::clone(&transaction));
        self.index.insert(hash, (lane_id, nonce));

        if let Some(replaced) = &replaced {
            let replaced_hash = *replaced.hash();
            self.hashes.remove(&replaced_hash);
            self.index.remove(&replaced_hash);
        }

        let pending_len_after = lane.consecutive_pending_len();
        let state = if nonce < lane.next_nonce + pending_len_after as u64 {
            AddedTransactionState::Pending
        } else {
            AddedTransactionState::Queued(QueuedReason::NonceGap)
        };

        let promoted = if matches!(state, AddedTransactionState::Pending) {
            lane.consecutive_pending_transactions()
                .skip(pending_len_before)
                .filter(|candidate| *candidate.hash() != hash)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };

        // Maintain the expiry index after the final use of `lane` to avoid
        // overlapping the lane borrow with these `&mut self` helpers.
        self.track_expiry(&transaction, hash);
        if let Some(replaced) = &replaced {
            self.untrack_expiry(replaced, replaced.hash());
        }

        Ok(InsertOutcome { outcome: AddedTransactionOutcome { hash, state }, replaced, promoted })
    }

    /// Inserts a validated nonce-free (`NONCE_KEY_MAX`) EIP-8130 transaction.
    ///
    /// Nonce-free transactions are deduplicated by their signature-invariant
    /// replay identifier rather than the transaction hash: a collision on
    /// `replay_id` is rejected as [`PoolErrorKind::AlreadyImported`], which
    /// collapses re-signed `payer_auth`/`sender_auth` variants of one logical
    /// transaction. They are always pending and never form a lane.
    fn insert_nonce_free(
        &mut self,
        mut transaction: ValidPoolTransaction<T>,
        hash: TxHash,
        sender: Address,
        replay_id: B256,
    ) -> PoolResult<InsertOutcome<T>> {
        if self.nonce_free_txs.contains_key(&replay_id) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }

        // Bound the number of concurrent nonce-free transactions per sender.
        // Dedup by `replay_id` only collapses re-signed variants of one body;
        // distinct bodies each get a new `replay_id`, so without this cap a
        // single sender could grow the sidecar without limit.
        if self.nonce_free_count_by_sender.get(&sender).copied().unwrap_or(0)
            >= Self::MAX_NONCE_FREE_TXS_PER_SENDER
        {
            return Err(PoolError::new(hash, PoolErrorKind::SpammerExceededCapacity(sender)));
        }

        let sender_id = self.senders.sender_id_or_create(sender);
        // All nonce-free transactions carry `nonce_sequence == 0`, so they share
        // `transaction_id = (sender_id, 0)`. This collision is intentional: the
        // sidecar keys storage and dedup on `replay_id`/hash and never on
        // `transaction_id`, so no entries are lost. Downstream consumers must not
        // treat a nonce-free transaction's `transaction_id` as unique.
        transaction.transaction_id = TransactionId::new(sender_id, transaction.nonce());
        let transaction = Arc::new(transaction);

        self.track_expiry(&transaction, hash);
        self.nonce_free_txs.insert(replay_id, Arc::clone(&transaction));
        self.nonce_free_by_hash.insert(hash, replay_id);
        self.hashes.insert(hash, transaction);
        *self.nonce_free_count_by_sender.entry(sender).or_insert(0) += 1;

        Ok(InsertOutcome {
            outcome: AddedTransactionOutcome { hash, state: AddedTransactionState::Pending },
            replaced: None,
            promoted: Vec::new(),
        })
    }

    /// Removes the exact transactions by hash without advancing lane state.
    pub(crate) fn remove_transactions(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            if let Some(transaction) = self.remove_hash(*hash, false) {
                removed.push(transaction);
            }
        }
        removed
    }

    /// Removes transactions and their descendants for each hash.
    pub(crate) fn remove_transactions_and_descendants(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            let Some((lane_id, nonce)) = self.index.get(hash).copied() else {
                // Nonce-free transactions have no descendants; remove the single
                // transaction if present (lane lookup above misses them).
                removed.extend(self.remove_transactions(std::slice::from_ref(hash)));
                continue;
            };
            let Some(lane) = self.lanes.get(&lane_id) else {
                continue;
            };

            let descendant_hashes: Vec<_> = lane
                .transactions
                .range(nonce..)
                .map(|(_, transaction)| *transaction.hash())
                .collect();
            removed.extend(self.remove_transactions(&descendant_hashes));
        }
        removed
    }

    /// Prunes mined transactions and advances the matching lane heads.
    ///
    /// Nonce-free transactions carry no lane state, so they are simply removed
    /// by hash.
    pub(crate) fn prune_mined(&mut self, hashes: &[TxHash]) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();

        for hash in hashes {
            if self.nonce_free_by_hash.contains_key(hash)
                && let Some(transaction) = self.remove_hash(*hash, true)
            {
                removed.push(transaction);
            }
        }

        let mut ordered_hashes: Vec<_> = hashes
            .iter()
            .filter_map(|hash| {
                self.index.get(hash).map(|(lane_id, nonce)| (lane_id.0, lane_id.1, *nonce, *hash))
            })
            .collect();
        ordered_hashes.sort_unstable();

        for (_, _, _, hash) in ordered_hashes {
            if let Some(transaction) = self.remove_hash(hash, true) {
                removed.push(transaction);
            }
        }
        removed
    }

    /// Evicts EIP-8130 transactions whose `expiry` has passed relative to
    /// `now` (Unix seconds), returning the evicted transactions.
    ///
    /// Eviction is driven by [`Self::expiry_index`], which records only
    /// transactions with a non-zero `expiry`, so this is `O(k log n)` in the
    /// number of expired transactions rather than a full scan of the sidecar.
    /// Transactions with `expiry == 0` are never indexed and never expire.
    ///
    /// Upstream validation is expected to guarantee that nonce-free
    /// (`NONCE_KEY_MAX`) transactions carry a non-zero `expiry` (their only
    /// replay protection); this method does not re-enforce that, so a nonce-free
    /// transaction that somehow reached the sidecar with `expiry == 0` would
    /// never be evicted here. Expired transactions are removed exactly
    /// (descendants are left in place and simply become non-consecutive/queued).
    pub(crate) fn evict_expired(&mut self, now: u64) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let expired: Vec<TxHash> = self
            .expiry_index
            .range(..=now)
            .flat_map(|(_, hashes)| hashes.iter().copied())
            .collect();
        self.remove_transactions(&expired)
    }

    /// Removes all transactions for the given sender.
    pub(crate) fn remove_transactions_by_sender(
        &mut self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let hashes: Vec<_> =
            self.hashes.values().filter(|tx| tx.sender() == sender).map(|tx| *tx.hash()).collect();
        self.remove_transactions(&hashes)
    }

    /// Returns a best-transactions iterator snapshot.
    pub(crate) fn best_transactions<O>(
        &self,
        ordering: O,
        base_fee: u64,
    ) -> BestTwoDTransactions<T, O>
    where
        O: TransactionOrdering<Transaction = T>,
    {
        BestTwoDTransactions::new(&self.lanes, self.nonce_free_txs.values(), ordering, base_fee)
    }

    fn remove_hash(
        &mut self,
        hash: TxHash,
        advance_lane: bool,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        if let Some(replay_id) = self.nonce_free_by_hash.remove(&hash) {
            let transaction = self.nonce_free_txs.remove(&replay_id);
            self.hashes.remove(&hash);
            if let Some(transaction) = &transaction {
                self.untrack_expiry(transaction, &hash);
                self.decrement_nonce_free_count(transaction.sender());
            }
            return transaction;
        }

        let ((sender, nonce_key), nonce) = self.index.remove(&hash)?;
        let lane_id = (sender, nonce_key);
        let transaction = {
            let lane = self.lanes.get_mut(&lane_id)?;
            let transaction = lane.transactions.remove(&nonce)?;
            if advance_lane && nonce == lane.next_nonce {
                lane.next_nonce += 1;
            }
            transaction
        };

        if self.lanes.get(&lane_id).is_some_and(|lane| lane.transactions.is_empty()) {
            self.lanes.remove(&lane_id);
        }
        self.hashes.remove(&hash);
        self.untrack_expiry(&transaction, &hash);
        Some(transaction)
    }

    /// Returns the EIP-8130 `expiry` of `transaction`, if it is an EIP-8130
    /// transaction.
    fn expiry_of(transaction: &ValidPoolTransaction<T>) -> Option<u64> {
        Some(transaction.transaction.as_eip8130()?.tx().expiry)
    }

    /// Records `hash` in [`Self::expiry_index`] when `transaction` carries a
    /// non-zero `expiry`. Transactions with `expiry == 0` are never indexed and
    /// therefore never expire.
    fn track_expiry(&mut self, transaction: &ValidPoolTransaction<T>, hash: TxHash) {
        if let Some(expiry) = Self::expiry_of(transaction)
            && expiry != 0
        {
            self.expiry_index.entry(expiry).or_default().insert(hash);
        }
    }

    /// Removes `hash` from [`Self::expiry_index`]; a no-op if it was never
    /// indexed (e.g. `expiry == 0`).
    fn untrack_expiry(&mut self, transaction: &ValidPoolTransaction<T>, hash: &TxHash) {
        if let Some(expiry) = Self::expiry_of(transaction)
            && expiry != 0
            && let Some(bucket) = self.expiry_index.get_mut(&expiry)
        {
            bucket.remove(hash);
            if bucket.is_empty() {
                self.expiry_index.remove(&expiry);
            }
        }
    }

    /// Decrements the per-sender nonce-free count, dropping the entry at zero.
    fn decrement_nonce_free_count(&mut self, sender: Address) {
        if let Some(count) = self.nonce_free_count_by_sender.get_mut(&sender) {
            *count -= 1;
            if *count == 0 {
                self.nonce_free_count_by_sender.remove(&sender);
            }
        }
    }
}

/// Snapshot iterator over the current best transactions of the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct BestTwoDTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    lanes: Vec<LaneIterator<T>>,
    nonce_free: Vec<NonceFreeCandidate<T>>,
    ordering: O,
    base_fee: u64,
}

#[derive(Debug)]
struct LaneIterator<T: BasePooledTx> {
    id: LaneId,
    transactions: Vec<Arc<ValidPoolTransaction<T>>>,
    index: usize,
    invalidated: bool,
}

/// A single always-pending nonce-free candidate in a best-transactions snapshot.
///
/// Unlike lane transactions, nonce-free transactions are independent: yielding
/// or invalidating one must not affect the others, so each is tracked
/// individually rather than collapsed into a `(sender, nonce_key)` lane.
#[derive(Debug)]
struct NonceFreeCandidate<T: BasePooledTx> {
    transaction: Arc<ValidPoolTransaction<T>>,
    yielded: bool,
    invalidated: bool,
}

impl<T: BasePooledTx, O> BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn new<'a>(
        lanes: &HashMap<LaneId, NonceLane<T>>,
        nonce_free: impl Iterator<Item = &'a Arc<ValidPoolTransaction<T>>>,
        ordering: O,
        base_fee: u64,
    ) -> Self
    where
        T: 'a,
    {
        let lanes = lanes
            .iter()
            .filter_map(|(id, lane)| {
                let mut next_nonce = lane.next_nonce;
                let mut transactions = Vec::new();
                while let Some(transaction) = lane.transactions.get(&next_nonce) {
                    transactions.push(Arc::clone(transaction));
                    let Some(incremented_nonce) = next_nonce.checked_add(1) else {
                        break;
                    };
                    next_nonce = incremented_nonce;
                }
                (!transactions.is_empty()).then(|| LaneIterator {
                    id: *id,
                    transactions,
                    index: 0,
                    invalidated: false,
                })
            })
            .collect();
        let nonce_free = nonce_free
            .map(|transaction| NonceFreeCandidate {
                transaction: Arc::clone(transaction),
                yielded: false,
                invalidated: false,
            })
            .collect();
        Self { lanes, nonce_free, ordering, base_fee }
    }

    fn priority_key(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> (Priority<O::PriorityValue>, Reverse<std::time::Instant>, TxHash) {
        (
            self.ordering.priority(&transaction.transaction, self.base_fee),
            Reverse(transaction.timestamp),
            *transaction.hash(),
        )
    }
}

impl<T: BasePooledTx, O> Iterator for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        type PriorityKey<O> = (
            Priority<<O as TransactionOrdering>::PriorityValue>,
            Reverse<std::time::Instant>,
            TxHash,
        );

        enum Pick {
            Lane(usize),
            NonceFree(usize),
        }

        let mut best: Option<(Pick, PriorityKey<O>)> = None;

        for (index, lane) in self.lanes.iter().enumerate() {
            if lane.invalidated || lane.index >= lane.transactions.len() {
                continue;
            }
            let key = self.priority_key(&lane.transactions[lane.index]);
            if best.as_ref().is_none_or(|(_, best_key)| key > *best_key) {
                best = Some((Pick::Lane(index), key));
            }
        }

        for (index, candidate) in self.nonce_free.iter().enumerate() {
            if candidate.invalidated || candidate.yielded {
                continue;
            }
            let key = self.priority_key(&candidate.transaction);
            if best.as_ref().is_none_or(|(_, best_key)| key > *best_key) {
                best = Some((Pick::NonceFree(index), key));
            }
        }

        match best?.0 {
            Pick::Lane(index) => {
                let lane = &mut self.lanes[index];
                let transaction = Arc::clone(&lane.transactions[lane.index]);
                lane.index += 1;
                Some(transaction)
            }
            Pick::NonceFree(index) => {
                let candidate = &mut self.nonce_free[index];
                candidate.yielded = true;
                Some(Arc::clone(&candidate.transaction))
            }
        }
    }
}

impl<T: BasePooledTx, O> BestTransactions for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: &InvalidPoolTransactionError) {
        // Nonce-free transactions are independent; invalidate only the matching
        // entry (by hash) so siblings from the same sender keep flowing.
        if transaction.transaction.is_eip8130_nonce_free() {
            let hash = *transaction.hash();
            if let Some(candidate) =
                self.nonce_free.iter_mut().find(|candidate| *candidate.transaction.hash() == hash)
            {
                candidate.invalidated = true;
            }
            return;
        }

        let Some(nonce_key) = transaction.transaction.eip8130_nonce_channel_key() else {
            return;
        };
        if let Some(lane) = self
            .lanes
            .iter_mut()
            .find(|lane| lane.id.0 == transaction.sender() && lane.id.1 == nonce_key)
        {
            lane.invalidated = true;
        }
    }

    fn no_updates(&mut self) {}

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_primitives::Bytes;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Constants, Eip8130Signed,
        TxEip8130,
    };
    use reth_transaction_pool::{PoolTransaction, PriceBumpConfig, TransactionOrigin};

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction};

    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn signed_channel_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_channel_tx_with_tip(signer, nonce_key, nonce_sequence, 0, max_fee_per_gas)
    }

    fn signed_channel_tx_with_tip(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            expiry: 0,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn signed_channel_tx_with_expiry(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        expiry: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            expiry,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn nonce_free_body(
        expiry: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> TxEip8130 {
        TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            payer: None,
        }
    }

    fn signed_nonce_free_tx(
        signer: &PrivateKeySigner,
        expiry: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_nonce_free_tx_with_tip(signer, expiry, 0, max_fee_per_gas)
    }

    fn signed_nonce_free_tx_with_tip(
        signer: &PrivateKeySigner,
        expiry: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = nonce_free_body(expiry, max_priority_fee_per_gas, max_fee_per_gas);
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    /// Builds a nonce-free transaction with the given body but an arbitrary
    /// `sender_auth`, simulating a re-signed/malleated authorization blob for
    /// the same logical transaction (same `resolved_sender`).
    fn nonce_free_with_auth(
        signer_address: Address,
        body: &TxEip8130,
        sender_auth: Bytes,
    ) -> BasePooledTransaction {
        let signed = Eip8130Signed::new(body.clone(), sender_auth, Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer_address))
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        valid_pool_transaction_at(transaction, Instant::now())
    }

    fn valid_pool_transaction_at(
        transaction: BasePooledTransaction,
        timestamp: Instant,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::External,
            authority_ids: None,
        }
    }

    #[test]
    fn channelized_transactions_with_same_sequence_can_coexist() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(2), 0, 1_000));

        pool.insert_validated(first).unwrap();
        pool.insert_validated(second).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!(pending, 2);
        assert_eq!(queued, 0);
        assert_eq!(pool.all_transactions().len(), 2);
    }

    #[test]
    fn same_channel_sequence_replacement_is_lane_local() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let original = valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_000));
        let replacement =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_250));
        let original_hash = *original.hash();
        let replacement_hash = *replacement.hash();

        pool.insert_validated(original).unwrap();
        let outcome = pool.insert_validated(replacement).unwrap();

        assert_eq!(
            outcome.replaced.as_ref().map(|transaction| *transaction.hash()),
            Some(original_hash)
        );
        assert!(pool.get(&original_hash).is_none());
        assert!(pool.get(&replacement_hash).is_some());
        assert_eq!(pool.all_transactions().len(), 1);
    }

    #[test]
    fn pruning_mined_head_promotes_next_sequence_in_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let head = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 0, 1_000));
        let head_hash = *head.hash();
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 1, 900));
        let queued_hash = *queued.hash();

        pool.insert_validated(head).unwrap();
        pool.insert_validated(queued).unwrap();

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (2, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![head_hash, queued_hash]
        );

        pool.prune_mined(&[head_hash]);

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (1, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![queued_hash]
        );
    }

    #[test]
    fn contiguous_lane_counts_full_run_as_pending() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 0, 1_000));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 1, 900));
        let third = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 2, 800));
        let gap = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 4, 700));

        let first_hash = *first.hash();
        let second_hash = *second.hash();
        let third_hash = *third.hash();
        let gap_hash = *gap.hash();

        pool.insert_validated(first).unwrap();
        pool.insert_validated(second).unwrap();
        pool.insert_validated(third).unwrap();
        pool.insert_validated(gap).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued), (3, 1));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![first_hash, second_hash, third_hash]
        );
        assert_eq!(
            pool.queued_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![gap_hash]
        );
    }

    #[test]
    fn queued_transactions_ignore_stale_nonces_below_lane_head() {
        let signer = signer();
        let stale =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 3, 1_000)));
        let first_pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 5, 900)));
        let second_pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 6, 800)));
        let queued =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 10, 700)));

        let lane = NonceLane {
            next_nonce: 5,
            transactions: BTreeMap::from([
                (3, Arc::clone(&stale)),
                (5, Arc::clone(&first_pending)),
                (6, Arc::clone(&second_pending)),
                (10, Arc::clone(&queued)),
            ]),
        };

        assert_eq!(
            lane.queued_transactions().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![*queued.hash()]
        );
    }

    #[test]
    fn consecutive_pending_handles_u64_max_nonce_without_overflow() {
        let signer = signer();
        let transaction = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(16),
            u64::MAX,
            1_000,
        )));

        let lane = NonceLane {
            next_nonce: u64::MAX,
            transactions: BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
        };

        assert_eq!(
            lane.consecutive_pending_transactions()
                .map(|transaction| *transaction.hash())
                .collect::<Vec<_>>(),
            vec![*transaction.hash()]
        );
        assert!(lane.queued_transactions().next().is_none());
    }

    #[test]
    fn best_transactions_snapshot_handles_u64_max_nonce_without_wrapping() {
        let signer = signer();
        let transaction = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(17),
            u64::MAX,
            1_000,
        )));
        let lane_id = (signer.address(), U256::from(17));
        let lanes = HashMap::from([(
            lane_id,
            NonceLane {
                next_nonce: u64::MAX,
                transactions: BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
            },
        )]);

        let mut best =
            BestTwoDTransactions::new(&lanes, std::iter::empty(), BaseOrdering::coinbase_tip(), 0);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(*transaction.hash()));
        assert!(best.next().is_none());
    }

    #[test]
    fn gap_fill_reports_newly_promoted_transactions() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 0, 1_000));
        let gap = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 2, 800));
        let middle = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 1, 900));
        let gap_hash = *gap.hash();

        pool.insert_validated(first).unwrap();
        pool.insert_validated(gap).unwrap();

        let outcome = pool.insert_validated(middle).unwrap();

        assert_eq!(
            outcome.promoted.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![gap_hash]
        );
    }

    #[test]
    fn pruning_mined_sorts_hashes_within_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 0, 1_000));
        let first_hash = *first.hash();
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 1, 900));
        let second_hash = *second.hash();
        let third = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 2, 800));
        let third_hash = *third.hash();
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 4, 700));

        pool.insert_validated(first).unwrap();
        pool.insert_validated(second).unwrap();
        pool.insert_validated(third).unwrap();
        pool.insert_validated(queued).unwrap();

        pool.prune_mined(&[third_hash, first_hash, second_hash]);

        let replacement =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 2, 850));
        let error = pool.insert_validated(replacement).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::InvalidTransaction(_)));
    }

    #[test]
    fn inserting_non_channelized_transaction_returns_error() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let non_channelized =
            valid_pool_transaction(signed_channel_tx(&signer, U256::ZERO, 0, 1_000));

        let error = pool.insert_validated(non_channelized).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::Other(_)));
    }

    #[test]
    fn mark_invalid_only_invalidates_matching_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first_lane_head =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(21), 0, 1_000));
        let first_lane_head_hash = *first_lane_head.hash();
        let first_lane_next =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(21), 1, 900));
        let second_lane_head =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(22), 0, 950));
        let second_lane_head_hash = *second_lane_head.hash();

        pool.insert_validated(first_lane_head).unwrap();
        pool.insert_validated(first_lane_next).unwrap();
        pool.insert_validated(second_lane_head).unwrap();

        let lane_to_invalidate = pool.get(&first_lane_head_hash).unwrap();
        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        best.mark_invalid(
            &lane_to_invalidate,
            &InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
        );

        let yielded_hashes: Vec<_> = best.map(|transaction| *transaction.hash()).collect();
        assert_eq!(yielded_hashes.len(), 1);
        assert_eq!(yielded_hashes[0], second_lane_head_hash);
    }

    #[test]
    fn best_transactions_uses_effective_tip_across_sidecar_lanes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let low_tip_high_cap =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(31), 0, 1, 100));
        let high_tip_lower_cap =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(32), 0, 50, 50));
        let high_tip_hash = *high_tip_lower_cap.hash();

        pool.insert_validated(low_tip_high_cap).unwrap();
        pool.insert_validated(high_tip_lower_cap).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 10);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(high_tip_hash));
    }

    #[test]
    fn equal_priority_prefers_earlier_submission_timestamp_across_sidecar_lanes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let now = Instant::now();

        let older = valid_pool_transaction_at(
            signed_channel_tx_with_tip(&signer, U256::from(41), 0, 10, 100),
            now,
        );
        let older_hash = *older.hash();
        let newer = valid_pool_transaction_at(
            signed_channel_tx_with_tip(&signer, U256::from(42), 0, 10, 100),
            now + std::time::Duration::from_secs(1),
        );

        pool.insert_validated(older).unwrap();
        pool.insert_validated(newer).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 10);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(older_hash));
    }

    #[test]
    fn nonce_free_transactions_from_same_sender_coexist() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        // Distinct bodies (different fee caps) => distinct replay ids.
        let first = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000));
        let second = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 2_000));

        pool.insert_validated(first).unwrap();
        pool.insert_validated(second).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued), (2, 0));
        assert_eq!(pool.all_transactions().len(), 2);
    }

    #[test]
    fn nonce_free_dedup_rejects_resigned_variant_by_replay_id() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let body = nonce_free_body(100, 0, 1_000);

        // Same logical transaction, different sender_auth bytes => same replay
        // id but different transaction hash.
        let original = valid_pool_transaction(nonce_free_with_auth(
            signer.address(),
            &body,
            Bytes::from(vec![1u8; 65]),
        ));
        let resigned = valid_pool_transaction(nonce_free_with_auth(
            signer.address(),
            &body,
            Bytes::from(vec![2u8; 65]),
        ));
        assert_ne!(original.hash(), resigned.hash());

        pool.insert_validated(original).unwrap();
        let error = pool.insert_validated(resigned).unwrap_err();

        assert!(matches!(error.kind, PoolErrorKind::AlreadyImported));
        assert_eq!(pool.all_transactions().len(), 1);
    }

    #[test]
    fn nonce_free_and_channelized_transactions_coexist() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let nonce_free = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000));
        let channelized =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 0, 1_000));

        pool.insert_validated(nonce_free).unwrap();
        pool.insert_validated(channelized).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued), (2, 0));
        assert_eq!(pool.pending_transactions().len(), 2);
    }

    #[test]
    fn best_transactions_includes_nonce_free_ordered_by_tip() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let high_tip_nonce_free =
            valid_pool_transaction(signed_nonce_free_tx_with_tip(&signer, 1_000, 50, 200));
        let high_tip_hash = *high_tip_nonce_free.hash();
        let low_tip_channelized =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(61), 0, 10, 200));
        let low_tip_hash = *low_tip_channelized.hash();

        pool.insert_validated(high_tip_nonce_free).unwrap();
        pool.insert_validated(low_tip_channelized).unwrap();

        let yielded: Vec<_> = pool
            .best_transactions(BaseOrdering::coinbase_tip(), 10)
            .map(|transaction| *transaction.hash())
            .collect();
        assert_eq!(yielded, vec![high_tip_hash, low_tip_hash]);
    }

    #[test]
    fn mark_invalid_nonce_free_is_per_transaction() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000));
        let first_hash = *first.hash();
        let second = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 2_000));
        let second_hash = *second.hash();

        pool.insert_validated(first).unwrap();
        pool.insert_validated(second).unwrap();

        let to_invalidate = pool.get(&first_hash).unwrap();
        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        best.mark_invalid(
            &to_invalidate,
            &InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
        );

        let yielded: Vec<_> = best.map(|transaction| *transaction.hash()).collect();
        assert_eq!(yielded, vec![second_hash]);
    }

    #[test]
    fn prune_mined_removes_nonce_free_transaction() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let nonce_free = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000));
        let hash = *nonce_free.hash();
        pool.insert_validated(nonce_free).unwrap();

        let removed = pool.prune_mined(&[hash]);

        assert_eq!(
            removed.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![hash]
        );
        assert!(pool.get(&hash).is_none());
        assert_eq!(pool.all_transactions().len(), 0);
    }

    #[test]
    fn evict_expired_removes_only_expired_transactions() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let expiring_soon = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000));
        let expiring_soon_hash = *expiring_soon.hash();
        let expiring_later = valid_pool_transaction(signed_nonce_free_tx(&signer, 200, 2_000));
        let expiring_later_hash = *expiring_later.hash();
        // Channelized with expiry == 0 must never expire.
        let no_expiry =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(71), 0, 1_000));
        let no_expiry_hash = *no_expiry.hash();

        pool.insert_validated(expiring_soon).unwrap();
        pool.insert_validated(expiring_later).unwrap();
        pool.insert_validated(no_expiry).unwrap();

        let evicted = pool.evict_expired(150);

        assert_eq!(
            evicted.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![expiring_soon_hash]
        );
        assert!(pool.get(&expiring_soon_hash).is_none());
        assert!(pool.get(&expiring_later_hash).is_some());
        assert!(pool.get(&no_expiry_hash).is_some());
    }

    #[test]
    fn evict_expired_removes_channelized_with_non_zero_expiry() {
        // The expiry index covers channelized transactions too, not just
        // nonce-free ones, so a channelized tx with a non-zero expiry must be
        // evicted once the chain advances past it.
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let channel_expiring = valid_pool_transaction(signed_channel_tx_with_expiry(
            &signer,
            U256::from(5),
            0,
            100,
            1_000,
        ));
        let hash = *channel_expiring.hash();
        pool.insert_validated(channel_expiring).unwrap();

        let evicted = pool.evict_expired(150);

        assert_eq!(
            evicted.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![hash]
        );
        assert!(pool.get(&hash).is_none());
    }

    #[test]
    fn nonce_free_per_sender_cap_rejects_excess() {
        const CAP: usize = TwoDNoncePool::<BasePooledTransaction>::MAX_NONCE_FREE_TXS_PER_SENDER;
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        // Distinct fee caps => distinct bodies => distinct replay ids, so dedup
        // does not collapse them and each occupies a sidecar slot.
        for i in 0..CAP {
            let tx = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000 + i as u128));
            pool.insert_validated(tx).unwrap();
        }

        let excess =
            valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000 + CAP as u128));
        let error = pool.insert_validated(excess).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::SpammerExceededCapacity(_)));
        assert_eq!(pool.all_transactions().len(), CAP);
    }

    #[test]
    fn nonce_free_cap_slot_frees_after_removal() {
        const CAP: usize = TwoDNoncePool::<BasePooledTransaction>::MAX_NONCE_FREE_TXS_PER_SENDER;
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let mut first_hash = None;
        for i in 0..CAP {
            let tx = valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000 + i as u128));
            if i == 0 {
                first_hash = Some(*tx.hash());
            }
            pool.insert_validated(tx).unwrap();
        }

        // At capacity: a new distinct body is rejected.
        let blocked =
            valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000 + CAP as u128));
        assert!(matches!(
            pool.insert_validated(blocked).unwrap_err().kind,
            PoolErrorKind::SpammerExceededCapacity(_)
        ));

        // Free a slot, then the same new body is admitted.
        pool.remove_transactions(&[first_hash.unwrap()]);
        let admitted =
            valid_pool_transaction(signed_nonce_free_tx(&signer, 100, 1_000 + CAP as u128));
        pool.insert_validated(admitted).unwrap();
        assert_eq!(pool.all_transactions().len(), CAP);
    }

    #[test]
    fn nonce_free_cap_is_per_sender() {
        const CAP: usize = TwoDNoncePool::<BasePooledTransaction>::MAX_NONCE_FREE_TXS_PER_SENDER;
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let spammer = signer();
        let other = signer();

        for i in 0..CAP {
            let tx = valid_pool_transaction(signed_nonce_free_tx(&spammer, 100, 1_000 + i as u128));
            pool.insert_validated(tx).unwrap();
        }
        // The spammer is capped...
        let blocked = valid_pool_transaction(signed_nonce_free_tx(&spammer, 100, 9_000));
        assert!(matches!(
            pool.insert_validated(blocked).unwrap_err().kind,
            PoolErrorKind::SpammerExceededCapacity(_)
        ));
        // ...but an unrelated sender is unaffected.
        let other_tx = valid_pool_transaction(signed_nonce_free_tx(&other, 100, 1_000));
        pool.insert_validated(other_tx).unwrap();
        assert_eq!(pool.all_transactions().len(), CAP + 1);
    }
}
