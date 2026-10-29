//! Base transaction-pool wrapper that combines the reth protocol pool with an
//! EIP-8130 2D nonce sidecar and a separate validity pool.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
};

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::{BlobCellMask, BlobTransactionSidecarVariant},
};
use alloy_primitives::{Address, B256, TxHash, U256, map::AddressSet};
use base_common_precompiles::NonceManagerStorage;
use base_execution_eip8130::{FeeCheck, IntrinsicGas};
use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStore, BlobStoreError, BlockInfo, FullTransactionEvent,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool, PoolConfig, PoolResult,
    PoolSize, PoolTransaction, PoolUpdateKind, PropagatedTransactions, SubPool, TransactionEvents,
    TransactionListenerKind, TransactionOrigin, TransactionPool, TransactionPoolExt,
    TransactionValidationOutcome, TransactionValidationTaskExecutor, TransactionValidator,
    ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    pool::{AddedTransactionState, QueuedReason, TransactionEvent},
};
use tokio::{
    spawn,
    sync::mpsc::{self, error::TrySendError},
};
use tracing::debug;

use crate::{
    Admission, BasePooledTx, BaseTransactionValidator, FlashblockExpiry, GuardLimits, GuardMetrics,
    InvalidationCause, InvalidationKey, LimitRejection, MempoolGuard, ParkableBestTransactions,
    ParkableTransactionPool, ParkedBestTransactions, PoolStore, PredicateContext,
    StateDiffInvalidation, ValidityPoolLimits, ValidityPoolMetrics,
    best::{BridgedDescendants, MergeBestTransactions},
    two_d_nonce_pool::{
        InsertOutcome, LaneCursor, LaneId, LaneUpdate, SidecarLimits, TwoDNoncePool,
    },
};

const SIDE_CAR_EVENT_CHANNEL_SIZE: usize = 1024;

/// A per-account canonical state delta used for mempool invalidation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountStateDiff {
    /// Changed account (and contract address for storage slots).
    pub address: Address,
    /// New balance, when changed.
    pub balance: Option<U256>,
    /// New protocol nonce, when changed (`0` once the account is destroyed).
    pub nonce: Option<u64>,
    /// Whether the bytecode hash changed.
    pub code_changed: bool,
    /// Changed contract storage slots with their new values.
    pub changed_slots: Vec<(B256, U256)>,
}

impl AccountStateDiff {
    /// Creates an empty diff for `address`.
    #[must_use]
    pub fn new(address: Address) -> Self {
        Self { address, ..Default::default() }
    }

    /// Pushes the surfaces whose change invalidates every watcher. Nonce
    /// surfaces are excluded; see [`Self::push_nonce_advances`].
    fn push_exact_keys(&self, out: &mut Vec<InvalidationKey>) {
        if self.code_changed {
            out.push(InvalidationKey::CodeHash(self.address));
        }
        if self.address != NonceManagerStorage::ADDRESS {
            out.extend(
                self.changed_slots
                    .iter()
                    .map(|(slot, _)| InvalidationKey::Slot { address: self.address, slot: *slot }),
            );
        }
    }

    /// Pushes the nonce surfaces this diff advances, with their new values: the
    /// protocol nonce and every nonce manager slot. Including a lane head
    /// advances the nonce its successors are sequenced on, so these only
    /// invalidate the transactions left below the new value.
    fn push_nonce_advances(&self, out: &mut Vec<(InvalidationKey, u64)>) {
        if let Some(nonce) = self.nonce {
            out.push((InvalidationKey::ProtocolNonce(self.address), nonce));
        }
        if self.address == NonceManagerStorage::ADDRESS {
            out.extend(self.changed_slots.iter().map(|(slot, value)| {
                (
                    InvalidationKey::Slot { address: self.address, slot: *slot },
                    value.saturating_to(),
                )
            }));
        }
    }
}

/// A lane store ([`TwoDNoncePool`]) together with the listeners following its
/// transactions. Lock order within a store is pool, then listeners, then the
/// guard; across stores the sidecar is locked before the validity pool.
struct LaneStore<T: BasePooledTx> {
    pool: Arc<RwLock<TwoDNoncePool<T>>>,
    listeners: Arc<RwLock<SidecarListeners<T>>>,
}

impl<T: BasePooledTx> LaneStore<T> {
    fn new(pool: TwoDNoncePool<T>) -> Self {
        Self {
            pool: Arc::new(RwLock::new(pool)),
            listeners: Arc::new(RwLock::new(SidecarListeners::default())),
        }
    }
}

impl<T: BasePooledTx> Clone for LaneStore<T> {
    fn clone(&self) -> Self {
        Self { pool: Arc::clone(&self.pool), listeners: Arc::clone(&self.listeners) }
    }
}

/// A transaction occupying the same `(sender, nonce key, nonce)` slot as a
/// newcomer, but held by a different store than the one the newcomer targets.
struct CrossStoreReplacement<T: BasePooledTx> {
    store: PoolStore,
    transaction: Arc<ValidPoolTransaction<T>>,
}

/// How [`BaseTransactionPool::remove_from_stores`] treats what it removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveMode {
    /// Remove exactly the named transactions; the protocol pool parks their descendants.
    Exact,
    /// Remove the named transactions and every later one from the same sender.
    WithDescendants,
    /// The named transactions were mined: lane cursors advance past them.
    Mined,
}

/// Wrapper around reth's transaction pool that adds an EIP-8130 2D nonce
/// sidecar and a separate pool for transactions carrying validity predicates.
pub struct BaseTransactionPool<
    Client,
    S,
    Evm,
    T = crate::BasePooledTransaction,
    O = crate::BaseOrdering<T>,
> where
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    protocol_pool:
        Pool<TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>, O, S>,
    ordering: O,
    /// Standard EIP-8130 transactions on finite nonce channels or nonce-free.
    sidecar: LaneStore<T>,
    /// Every transaction carrying validity predicates, in lanes keyed by
    /// `(sender, nonce key)` with the protocol nonce as key zero.
    validity: LaneStore<T>,
    /// Shared admission and invalidation ledger for EIP-8130 transactions.
    guard: Arc<RwLock<MempoolGuard>>,
    /// Block and flashblock deadline index for validity transactions, including
    /// EIP-1559 advanced submissions.
    block_expiry: Arc<RwLock<crate::BlockExpiryIndex>>,
    /// Highest published position on the current branch; checked under `protocol_admission_lock`.
    published_flashblock: Arc<RwLock<Option<PredicateContext>>>,
    /// Serializes pool admission with successful flashblock publication and
    /// deadline eviction, so a same-nonce submission cannot race the sweep.
    /// Validation itself remains outside this lock.
    protocol_admission_lock: Arc<Mutex<()>>,
}

impl<Client, S, Evm, T, O> fmt::Debug for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseTransactionPool").finish_non_exhaustive()
    }
}

impl<Client, S, Evm, T, O> Clone for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn clone(&self) -> Self {
        Self {
            protocol_pool: self.protocol_pool.clone(),
            ordering: self.ordering.clone(),
            sidecar: self.sidecar.clone(),
            validity: self.validity.clone(),
            guard: Arc::clone(&self.guard),
            block_expiry: Arc::clone(&self.block_expiry),
            published_flashblock: Arc::clone(&self.published_flashblock),
            protocol_admission_lock: Arc::clone(&self.protocol_admission_lock),
        }
    }
}

impl<Client, S, Evm, T, O> Unpin for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
}

impl<Client, S, Evm, T, O> BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    /// Creates a new wrapper around the reth protocol pool. The validity pool
    /// starts with [`ValidityPoolLimits::inherit`]; see
    /// [`Self::with_validity_pool_limits`].
    pub fn new(
        protocol_pool: Pool<
            TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>,
            O,
            S,
        >,
        ordering: O,
    ) -> Self {
        let config = protocol_pool.config();
        let sidecar = TwoDNoncePool::new(config.price_bumps)
            .with_limits(SidecarLimits::from_pool_config(config));
        let validity = Self::validity_pool(config, ValidityPoolLimits::inherit());
        Self {
            protocol_pool,
            ordering,
            sidecar: LaneStore::new(sidecar),
            validity: LaneStore::new(validity),
            guard: Arc::new(RwLock::new(MempoolGuard::unlimited())),
            block_expiry: Arc::new(RwLock::new(crate::BlockExpiryIndex::new())),
            published_flashblock: Arc::new(RwLock::new(None)),
            protocol_admission_lock: Arc::new(Mutex::new(())),
        }
    }

    fn validity_pool(config: &PoolConfig, limits: ValidityPoolLimits) -> TwoDNoncePool<T> {
        TwoDNoncePool::new(config.price_bumps)
            .with_limits(limits.resolve(config))
            .with_protocol_nonce_lanes()
    }

    /// Configures EIP-8130 admission limits. Call before sharing the pool.
    #[must_use]
    pub fn with_guard_limits(self, limits: GuardLimits) -> Self {
        *self.guard.write() = MempoolGuard::new(limits);
        self
    }

    /// Configures the validity pool's capacity. Call before sharing the pool.
    #[must_use]
    pub fn with_validity_pool_limits(self, limits: ValidityPoolLimits) -> Self {
        *self.validity.pool.write() = Self::validity_pool(self.protocol_pool.config(), limits);
        self
    }

    /// Builds guard admission metadata carried by a validated EIP-8130 transaction.
    pub fn admission_for(transaction: &T) -> Option<Admission> {
        transaction.as_eip8130()?;
        let watch_set = transaction.watch_set().cloned()?;
        let class = *transaction.limit_class()?;
        Some(Admission {
            hash: *transaction.hash(),
            sender: class.sender,
            payer: class.payer,
            sender_locked: class.sender_locked,
            payer_locked: class.payer_locked,
            payer_trusted: class.payer_trusted,
            payer_allowlisted: class.payer_allowlisted,
            payer_balance: class.payer_balance,
            max_cost: class.max_cost,
            priority: transaction.priority_fee_or_price(),
            watch_set,
        })
    }

    /// Returns the wrapped reth pool.
    pub const fn protocol_pool(
        &self,
    ) -> &Pool<TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>, O, S>
    {
        &self.protocol_pool
    }

    /// Returns the validator backing the wrapped reth pool.
    pub fn validator(
        &self,
    ) -> &TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>> {
        self.protocol_pool.validator()
    }

    /// Returns the number of transactions and their summed size held by the
    /// validity pool.
    pub fn validity_pool_occupancy(&self) -> (usize, usize) {
        let validity = self.validity.pool.read();
        (validity.len(), validity.size())
    }

    /// The lane store behind `store`, or `None` for the protocol pool.
    const fn lane_store(&self, store: PoolStore) -> Option<&LaneStore<T>> {
        match store {
            PoolStore::Protocol => None,
            PoolStore::Sidecar => Some(&self.sidecar),
            PoolStore::Validity => Some(&self.validity),
        }
    }

    /// Both lane stores, in lock order.
    const fn lane_stores(&self) -> [&LaneStore<T>; 2] {
        [&self.sidecar, &self.validity]
    }

    /// The lane a non-nonce-free transaction is sequenced in: its EIP-8130
    /// channel, or nonce key zero for the account's protocol nonce.
    fn lane_id_of(transaction: &T) -> Option<LaneId> {
        if transaction.is_eip8130_nonce_free() {
            return None;
        }
        Some((transaction.sender(), transaction.eip8130_nonce_channel_key().unwrap_or(U256::ZERO)))
    }

    fn limit_rejection_error(hash: TxHash, rejection: LimitRejection) -> PoolError {
        GuardMetrics::admission_rejected(GuardMetrics::rejection_reason(rejection)).increment(1);
        let reason = match rejection {
            LimitRejection::SenderLimit => "sender EIP-8130 signature limit reached",
            LimitRejection::PayerLimit => "payer EIP-8130 signature limit reached",
            LimitRejection::PaymentLimit => "payer EIP-8130 payment limit reached",
            LimitRejection::PayerBalance => "payer cannot fund another EIP-8130 transaction",
        };
        debug!(reason = GuardMetrics::rejection_reason(rejection), "EIP-8130 admission rejected");
        PoolError::other(hash, reason)
    }

    /// Applies canonical state changes and evicts affected EIP-8130 transactions.
    pub fn apply_state_diff(
        &self,
        diffs: &[AccountStateDiff],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        // Keep classification-generation changes atomic with protocol admission.
        let _admission_guard = self.protocol_admission_lock.lock();
        // Advance validator classification generation before dropping guard records.
        self.validator().validator().invalidate_limit_class_cache(diffs);
        let mut exact = Vec::new();
        let mut advances = Vec::new();
        for diff in diffs {
            diff.push_exact_keys(&mut exact);
            diff.push_nonce_advances(&mut advances);
        }
        for store in self.lane_stores() {
            store.pool.write().apply_balance_updates(
                diffs.iter().filter_map(|diff| Some((diff.address, diff.balance?))),
            );
        }
        let (behind, lane_moves) = self.resolve_nonce_advances(&advances);
        // Move lane cursors before releasing what fell behind them, so a
        // successor that stays pending is not re-announced as promoted.
        let mut removed = Vec::new();
        let mut validity_lanes = Vec::new();
        if !lane_moves.is_empty() {
            let mut sidecar = self.sidecar.pool.write();
            let mut update = LaneUpdate::default();
            for (store, lane_id, nonce) in lane_moves {
                match store {
                    PoolStore::Sidecar => {
                        update.extend(sidecar.advance_lane_nonce(lane_id, nonce));
                        // A validity lane on the same channel follows the sidecar's run.
                        validity_lanes.push((lane_id, None));
                    }
                    PoolStore::Validity => validity_lanes.push((lane_id, Some(nonce))),
                    PoolStore::Protocol => {}
                }
            }
            self.publish_lane_update(&self.sidecar, &update);
            removed.extend(update.discarded);
        }
        // Protocol nonce advances re-anchor the validity lanes sequenced on them,
        // whether or not a guarded transaction watches that nonce.
        validity_lanes.extend(advances.iter().filter_map(|(key, nonce)| match key {
            InvalidationKey::ProtocolNonce(account) => Some(((*account, U256::ZERO), Some(*nonce))),
            _ => None,
        }));
        removed.extend(self.sync_validity_lanes(validity_lanes));
        let dropped = {
            let mut guard = self.guard.write();
            let mut dropped = guard.invalidate_exact(exact);
            dropped.extend(behind.into_iter().filter(|hash| guard.release(hash)));
            for diff in diffs {
                if let Some(balance) = diff.balance {
                    dropped.extend(guard.on_balance_changed(diff.address, balance));
                }
            }
            dropped
        };
        removed.extend(self.remove_from_stores(dropped, RemoveMode::Exact));
        GuardMetrics::record_state_diff_invalidations(removed.len());
        if !removed.is_empty() {
            debug!(count = removed.len(), "EIP-8130 transactions invalidated by state diff");
        }
        removed
    }

    /// Clears every guarded transaction, attributing the flush to `cause` for
    /// metrics. Used for the two fail-safe bulk paths (reorg and feed gap).
    pub fn invalidate_all_tracked_transactions(
        &self,
        cause: InvalidationCause,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        // Keep classification-generation changes atomic with protocol admission.
        let _admission_guard = self.protocol_admission_lock.lock();
        self.validator().validator().clear_limit_class_cache();
        let dropped = self.guard.write().invalidate_all();
        let removed = self.remove_from_stores(dropped, RemoveMode::Exact);
        GuardMetrics::record_bulk_invalidations(removed.len(), cause);
        removed
    }

    /// Resolves the watchers of each nonce advance. A watcher survives only when
    /// it is an EIP-8130 transaction sequenced on that nonce at or after the new
    /// value; every other watcher is returned to be invalidated, as an exact
    /// match would. Each surviving lane is returned with the store holding it
    /// and the nonce its cursor moves to.
    fn resolve_nonce_advances(
        &self,
        advances: &[(InvalidationKey, u64)],
    ) -> (Vec<TxHash>, Vec<(PoolStore, LaneId, u64)>) {
        let mut behind = Vec::new();
        let mut lane_moves = Vec::new();
        if advances.is_empty() {
            return (behind, lane_moves);
        }
        let watchers: Vec<_> = {
            let guard = self.guard.read();
            advances.iter().map(|(key, nonce)| (key, *nonce, guard.watchers(key))).collect()
        };
        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        for (key, nonce, hashes) in watchers {
            let mut moved: Vec<(PoolStore, LaneId)> = Vec::new();
            for hash in hashes {
                let (store, transaction) = sidecar
                    .get(&hash)
                    .map(|transaction| (Some(PoolStore::Sidecar), Some(transaction)))
                    .or_else(|| {
                        validity
                            .get(&hash)
                            .map(|transaction| (Some(PoolStore::Validity), Some(transaction)))
                    })
                    .unwrap_or_else(|| (None, self.protocol_pool.get(&hash)));
                let lane = transaction.and_then(|transaction| {
                    let tx = transaction.transaction.as_eip8130()?.tx();
                    let lane = (transaction.sender(), tx.nonce_key);
                    let sequenced_on_key = match key {
                        InvalidationKey::ProtocolNonce(account) => {
                            tx.nonce_key.is_zero() && lane.0 == *account
                        }
                        InvalidationKey::Slot { slot, .. } => {
                            NonceManagerStorage::nonce_slot(lane.0, tx.nonce_key)
                                .is_ok_and(|channel| B256::from(channel) == *slot)
                        }
                        _ => false,
                    };
                    (sequenced_on_key && tx.nonce_sequence >= nonce).then_some(lane)
                });
                match (lane, store) {
                    (Some(lane), Some(store)) => {
                        if !moved.contains(&(store, lane)) {
                            moved.push((store, lane));
                        }
                    }
                    (Some(_), None) => {}
                    (None, _) => behind.push(hash),
                }
            }
            lane_moves.extend(moved.into_iter().map(|(store, lane)| (store, lane, nonce)));
        }
        (behind, lane_moves)
    }

    /// Publishes a lane update of `store` and frees the guard capacity its
    /// discarded transactions held. Callers hold the store's pool lock so the
    /// events stay ordered with the lane change.
    fn publish_lane_update(&self, store: &LaneStore<T>, update: &LaneUpdate<T>) {
        if update.discarded.is_empty() && update.promoted.is_empty() && update.queued.is_empty() {
            return;
        }
        store.listeners.write().on_lane_update(update);
        self.release_from_guard(&update.discarded);
    }

    /// Re-anchors validity lanes after a standard store or canonical state
    /// moved. Each entry names a lane and, when known, the canonical nonce it
    /// now sits at (anchors only move forward this way); the executable cursor
    /// then follows the standard store's pending run from that anchor. Returns
    /// the transactions discarded for falling below a cursor.
    ///
    /// Runs under `protocol_admission_lock`: the target is derived from the
    /// standard stores' state without holding the validity lock, so a
    /// concurrent admission anchoring the same lane could otherwise be
    /// overwritten by a stale cursor and lose its transaction.
    fn sync_validity_lanes(
        &self,
        lanes: impl IntoIterator<Item = (LaneId, Option<u64>)>,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        debug_assert!(
            self.protocol_admission_lock.is_locked(),
            "validity lanes are re-anchored under the admission lock"
        );
        let mut targets: HashMap<LaneId, Option<u64>> = HashMap::new();
        for (lane_id, nonce) in lanes {
            let entry = targets.entry(lane_id).or_default();
            *entry = (*entry).max(nonce);
        }
        let mut discarded = Vec::new();
        for (lane_id, nonce) in targets {
            let (cursor, shared) = {
                let validity = self.validity.pool.read();
                let Some(cursor) = validity.lane_cursor(&lane_id) else { continue };
                (cursor, validity.is_shared_lane(&lane_id))
            };
            // Every change to a lane's standard run passes through here, so
            // this is where a shared flag is dropped once the standard store
            // holds nothing in the lane; the next admission in it re-flags.
            if shared && !self.standard_store_holds_lane(lane_id) {
                self.validity.pool.write().unmark_shared_lane(&lane_id);
            }
            let state_nonce =
                nonce.map_or(cursor.state_nonce, |nonce| nonce.max(cursor.state_nonce));
            let target = LaneCursor {
                state_nonce,
                next_nonce: self.standard_pending_end(lane_id, state_nonce),
            };
            if target == cursor {
                continue;
            }
            let mut validity = self.validity.pool.write();
            let update = validity.set_lane_cursor(lane_id, target);
            self.publish_lane_update(&self.validity, &update);
            drop(validity);
            discarded.extend(update.discarded);
        }
        discarded
    }

    /// The nonce right after the standard store's consecutive pending run that
    /// starts at `from` in `lane_id`, or `from` when no such run starts there.
    /// Validity transactions behind that run are executable; those behind a gap
    /// or a parked standard transaction are not.
    fn standard_pending_end(&self, lane_id: LaneId, from: u64) -> u64 {
        let (sender, nonce_key) = lane_id;
        if nonce_key.is_zero() {
            if self.protocol_pool.get_transaction_by_sender_and_nonce(sender, from).is_none() {
                return from;
            }
            let Some(head) =
                self.protocol_pool.get_highest_consecutive_transaction_by_sender(sender, from)
            else {
                return from;
            };
            // The run is executable only if its head is pending: a parked head
            // means a parked ancestor, a fee or balance shortfall, or a gap.
            let head_nonce = head.nonce();
            if self
                .protocol_pool
                .get_pending_transaction_by_sender_and_nonce(sender, head_nonce)
                .is_some()
            {
                head_nonce.saturating_add(1).max(from)
            } else {
                from
            }
        } else {
            let sidecar = self.sidecar.pool.read();
            if sidecar.lane_transaction(&lane_id, from).is_none() {
                return from;
            }
            sidecar.lane_pending_end(&lane_id).map_or(from, |end| end.max(from))
        }
    }

    /// Splits `hashes` by the store currently holding each, in [`PoolStore::ALL`]
    /// order. Unknown hashes are attributed to the protocol pool.
    fn partition_hashes_by_store(&self, hashes: Vec<TxHash>) -> [Vec<TxHash>; 3] {
        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        let mut partitioned: [Vec<TxHash>; 3] = Default::default();
        for hash in hashes {
            let store = if sidecar.contains(&hash) {
                PoolStore::Sidecar
            } else if validity.contains(&hash) {
                PoolStore::Validity
            } else {
                PoolStore::Protocol
            };
            partitioned[store.index()].push(hash);
        }
        partitioned
    }

    /// Removes `hashes` from whichever store holds each, publishes the matching
    /// listener events, frees guard capacity and re-anchors the validity lanes
    /// whose standard run changed. Returns everything removed, including
    /// validity transactions discarded by a lane re-anchor.
    fn remove_from_stores(
        &self,
        hashes: Vec<TxHash>,
        mode: RemoveMode,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        if hashes.is_empty() {
            return Vec::new();
        }
        let [protocol_hashes, sidecar_hashes, validity_hashes] =
            self.partition_hashes_by_store(hashes);
        let mut removed = if protocol_hashes.is_empty() {
            Vec::new()
        } else {
            match mode {
                RemoveMode::Exact => self.protocol_pool.remove_transactions(protocol_hashes),
                RemoveMode::WithDescendants => {
                    self.protocol_pool.remove_transactions_and_descendants(protocol_hashes)
                }
                RemoveMode::Mined => self.protocol_pool.prune_transactions(protocol_hashes),
            }
        };
        // A mined transaction proves its lane's canonical nonce moved past it;
        // any other removal leaves the anchor alone and only re-checks the run.
        let anchor = |transaction: &Arc<ValidPoolTransaction<T>>| {
            (mode == RemoveMode::Mined).then(|| transaction.nonce().saturating_add(1))
        };
        let mut touched_lanes: Vec<(LaneId, Option<u64>)> = removed
            .iter()
            .map(|transaction| ((transaction.sender(), U256::ZERO), anchor(transaction)))
            .collect();
        let sidecar_removed = self.remove_from_lane_store(&self.sidecar, &sidecar_hashes, mode);
        touched_lanes.extend(sidecar_removed.iter().filter_map(|transaction| {
            Self::lane_id_of(&transaction.transaction).map(|lane_id| (lane_id, anchor(transaction)))
        }));
        removed.extend(sidecar_removed);
        let validity_removed = self.remove_from_lane_store(&self.validity, &validity_hashes, mode);
        if mode == RemoveMode::Mined {
            // A mined validity transaction advanced its own lane's anchor; the
            // cursor must now also follow whatever standard run begins there.
            touched_lanes.extend(validity_removed.iter().filter_map(|transaction| {
                Self::lane_id_of(&transaction.transaction)
                    .map(|lane_id| (lane_id, anchor(transaction)))
            }));
        }
        removed.extend(validity_removed);
        self.release_from_guard(&removed);
        removed.extend(self.sync_validity_lanes(touched_lanes));
        removed
    }

    /// Removes `hashes` from one lane store and publishes the listener events
    /// for what changed. Lane-cursor discards are included in the result.
    fn remove_from_lane_store(
        &self,
        store: &LaneStore<T>,
        hashes: &[TxHash],
        mode: RemoveMode,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        if hashes.is_empty() {
            return Vec::new();
        }
        let mut pool = store.pool.write();
        match mode {
            RemoveMode::Exact => {
                let removed = pool.remove_transactions(hashes);
                if !removed.is_empty() {
                    store.listeners.write().on_discarded(&removed);
                }
                removed
            }
            RemoveMode::WithDescendants => {
                let removed = pool.remove_transactions_and_descendants(hashes);
                if !removed.is_empty() {
                    store.listeners.write().on_discarded(&removed);
                }
                removed
            }
            RemoveMode::Mined => {
                let mut pruned = pool.prune_mined(hashes);
                store.listeners.write().on_lane_update(&pruned.lanes);
                pruned.removed.append(&mut pruned.lanes.discarded);
                pruned.removed
            }
        }
    }

    fn release_from_guard(&self, removed: &[Arc<ValidPoolTransaction<T>>]) {
        if removed.is_empty() {
            return;
        }
        let mut guard = self.guard.write();
        for transaction in removed {
            guard.release(transaction.hash());
        }
    }

    /// Invalidates guarded transactions whose effective expiry has elapsed as of
    /// `now` (the committed block timestamp).
    ///
    /// The horizon is `now / EXPIRY_BUCKET_SECS`, so a bucket fires once its
    /// window has started — no forward lookahead, which would over-evict still
    /// valid transactions when blocks are produced faster than the bucket width.
    /// Driven from `on_canonical_state_change`, so eviction rides block cadence;
    /// inclusion safety does not depend on it, since the builder's manifest
    /// precheck drops any past-expiry transaction against the exact build
    /// timestamp. This path is pure guard hygiene: it frees `PayerBook`
    /// reservations and count-cap slots held by expired transactions.
    fn expire_due_buckets(&self, now: u64) {
        // Do not expire a protocol reservation between guard and reth insertion.
        let _admission_guard = self.protocol_admission_lock.lock();
        let horizon = now / InvalidationKey::EXPIRY_BUCKET_SECS;
        let (dropped, bucket_count) = self.guard.write().invalidate_expiry_buckets_through(horizon);
        GuardMetrics::expiry_buckets_fired().increment(bucket_count as u64);
        let removed = self.remove_from_stores(dropped, RemoveMode::Exact);
        GuardMetrics::record_expiry_invalidations(removed.len());
        if !removed.is_empty() {
            debug!(count = removed.len(), "EIP-8130 transactions invalidated by expiry");
        }
    }

    /// Returns the block-expiry bound for a validated transaction's validity
    /// predicates, or `None` when they impose no finite block bound.
    fn validity_block_expiry_bound(validated: &TransactionValidationOutcome<T>) -> Option<u64> {
        let transaction = validated.as_valid_transaction()?;
        transaction.transaction().validity_conditions().block().expiry_bound()
    }

    /// Optional flashblock upper bound, paired with the block bound for pool eviction.
    fn validity_flashblock_expiry_bound(
        validated: &TransactionValidationOutcome<T>,
    ) -> Option<u64> {
        let transaction = validated.as_valid_transaction()?;
        transaction.transaction().validity_conditions().flashblock().expiry_bound()
    }

    /// Rejects a late arrival whose deadline is behind the published position.
    /// Called while holding `protocol_admission_lock`, on every admission path.
    fn ensure_not_published_expired(
        &self,
        hash: TxHash,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<()> {
        let published = *self.published_flashblock.read();
        if let (Some(position), Some(transaction)) = (published, validated.as_valid_transaction()) {
            let next = PredicateContext {
                block_number: position.block_number,
                flashblock_index: position.flashblock_index.saturating_add(1),
            };
            if transaction.transaction().validity_conditions().is_expired(&next) {
                return Err(PoolError::other(
                    hash,
                    "validity deadline expired at published flashblock",
                ));
            }
        }
        Ok(())
    }

    /// Rechecks an EIP-8130 transaction against the pool's block gas limit, which can
    /// move after validation, as reth's protocol pool does for every transaction at
    /// insert. Execution reserves `gas_limit + payer_auth` against the block, and an
    /// unpriceable payer authentication never fits.
    fn ensure_eip8130_fits_block_gas_limit(
        &self,
        hash: TxHash,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<()> {
        let Some(transaction) = validated.as_valid_transaction().map(|valid| valid.transaction())
        else {
            return Ok(());
        };
        let Some(signed) = transaction.as_eip8130() else {
            return Ok(());
        };
        let reserved_gas = IntrinsicGas::max_payer_auth_cost(signed)
            .map_or(u64::MAX, |payer_auth| {
                FeeCheck::max_chargeable_gas(transaction.gas_limit(), payer_auth)
            });
        let block_gas_limit = self.protocol_pool.block_info().block_gas_limit;
        if reserved_gas > block_gas_limit {
            return Err(PoolError::new(
                hash,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsGasLimit(
                    reserved_gas,
                    block_gas_limit,
                )),
            ));
        }
        Ok(())
    }

    /// Records a transaction's last valid block in the block-expiry index,
    /// dropping any replaced transaction's stale entry in the same pass so a
    /// fee-bump replacement does not orphan the old hash until its expiry block.
    fn register_block_expiry(
        &self,
        hash: TxHash,
        last_valid_block: Option<u64>,
        last_valid_flashblock: Option<u64>,
        replaced: Option<TxHash>,
    ) {
        if last_valid_block.is_none() && replaced.is_none() {
            return;
        }
        let mut block_expiry = self.block_expiry.write();
        if let Some(replaced) = replaced {
            block_expiry.remove(&replaced);
        }
        if let Some(last_valid_block) = last_valid_block {
            block_expiry.insert_with_flashblock(hash, last_valid_block, last_valid_flashblock);
        }
    }

    /// Evicts validity-predicate transactions whose last valid block is at or
    /// before the newly committed `committed_block`, since the next block cannot include them.
    ///
    /// Canonical-state maintenance catches block-only deadlines and transactions
    /// missed by flashblock publication. Entries removed through inclusion or
    /// replacement remain indexed until their deadline, bounding index growth
    /// to the furthest accepted block bound.
    fn expire_by_block(&self, committed_block: u64) {
        let _admission_guard = self.protocol_admission_lock.lock();
        let expired = self.block_expiry.write().drain_expired(committed_block);
        // `remove_from_stores` also frees any guard slot directly rather than
        // deferring to the reconciliation sweep: an EIP-8130 transaction may
        // carry block_number validity predicates, so an evicted tx can also
        // hold guard capacity.
        let removed = self.remove_from_stores(expired, RemoveMode::Exact);
        if !removed.is_empty() {
            GuardMetrics::record_block_expiry_invalidations(removed.len());
            debug!(
                count = removed.len(),
                block = committed_block,
                "validity transactions invalidated by block expiry"
            );
        }
    }

    /// Evicts indexed deadlines while the admission lock is held.
    fn expire_published_locked(&self, block_number: u64, flashblock_index: u64) -> usize {
        let position = PredicateContext { block_number, flashblock_index };
        let mut published = self.published_flashblock.write();
        if published.is_some_and(|previous| {
            (previous.block_number, previous.flashblock_index) >= (block_number, flashblock_index)
        }) {
            return 0;
        }
        *published = Some(position);
        drop(published);
        let expired = self.block_expiry.write().drain_published(block_number, flashblock_index);
        let removed = self.remove_from_stores(expired, RemoveMode::Exact);
        GuardMetrics::record_block_expiry_invalidations(removed.len());
        if !removed.is_empty() {
            debug!(
                block = block_number,
                flashblock = flashblock_index,
                count = removed.len(),
                "validity transactions expired at published flashblock"
            );
        }
        removed.len()
    }

    fn reconcile_guard(&self) {
        let tracked = self.guard.read().tracked_hashes();
        if tracked.is_empty() {
            return;
        }
        let stale = self.untracked_in_every_store(tracked);
        if stale.is_empty() {
            return;
        }

        // Recheck only stale candidates under admission serialization. The
        // initial full scan stays outside this critical section, while a
        // reserved-before-insert transaction cannot be mistaken for an orphan.
        let _admission_guard = self.protocol_admission_lock.lock();
        let stale = self.untracked_in_every_store(stale);
        if stale.is_empty() {
            return;
        }
        let mut guard = self.guard.write();
        for hash in &stale {
            guard.release(hash);
        }
        drop(guard);
        GuardMetrics::record_reconcile_releases(stale.len());
    }

    /// Filters `hashes` down to those no store holds.
    fn untracked_in_every_store(&self, hashes: Vec<TxHash>) -> Vec<TxHash> {
        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        hashes
            .into_iter()
            .filter(|hash| {
                self.protocol_pool.get(hash).is_none()
                    && !sidecar.contains(hash)
                    && !validity.contains(hash)
            })
            .collect()
    }

    fn protocol_replacement_hash(&self, sender: Address, nonce: u64) -> Option<TxHash> {
        self.protocol_pool
            .get_transaction_by_sender_and_nonce(sender, nonce)
            .map(|existing| *existing.hash())
    }

    /// Finds the transaction another store holds in `transaction`'s slot, so a
    /// newcomer bound for `target` replaces it under the usual fee-bump rule
    /// instead of coexisting with it. The protocol pool and validity pool share
    /// protocol-nonce slots; the sidecar and validity pool share channel slots
    /// and nonce-free replay ids.
    fn cross_store_replacement(
        &self,
        target: PoolStore,
        transaction: &T,
    ) -> PoolResult<Option<CrossStoreReplacement<T>>> {
        let sender = transaction.sender();
        let nonce = transaction.nonce();
        let found = match target {
            PoolStore::Protocol => self
                .validity
                .pool
                .read()
                .lane_transaction(&(sender, U256::ZERO), nonce)
                .map(|existing| (PoolStore::Validity, existing)),
            PoolStore::Sidecar => Self::lane_slot(&self.validity.pool.read(), transaction)
                .map(|existing| (PoolStore::Validity, existing)),
            PoolStore::Validity => {
                if transaction.is_eip8130_sidecar_transaction() {
                    Self::lane_slot(&self.sidecar.pool.read(), transaction)
                        .map(|existing| (PoolStore::Sidecar, existing))
                } else {
                    self.protocol_pool
                        .get_transaction_by_sender_and_nonce(sender, nonce)
                        .map(|existing| (PoolStore::Protocol, existing))
                }
            }
        };
        let Some((store, existing)) = found else {
            return Ok(None);
        };
        let hash = *transaction.hash();
        if *existing.hash() == hash {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }
        let price_bumps = self.protocol_pool.config().price_bumps;
        if existing.transaction.is_replacement_underpriced(transaction, &price_bumps) {
            return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
        }
        Ok(Some(CrossStoreReplacement { store, transaction: existing }))
    }

    /// The transaction `pool` holds in `transaction`'s slot: the same replay id
    /// for a nonce-free transaction, otherwise the same channel and nonce.
    fn lane_slot(pool: &TwoDNoncePool<T>, transaction: &T) -> Option<Arc<ValidPoolTransaction<T>>> {
        if let Some(replay_id) = transaction.eip8130_replay_id() {
            return pool.nonce_free_transaction(&replay_id);
        }
        let nonce_key = transaction.eip8130_nonce_channel_key()?;
        pool.lane_transaction(&(transaction.sender(), nonce_key), transaction.nonce())
    }

    /// Removes the transaction `replaced_by` superseded from the store that
    /// held it. Lane stores announce the replacement; the protocol pool
    /// announces a discard and parks the replaced transaction's descendants
    /// behind the newcomer.
    fn remove_cross_store_replaced(&self, cross: CrossStoreReplacement<T>, replaced_by: TxHash) {
        let hash = *cross.transaction.hash();
        match self.lane_store(cross.store) {
            None => {
                self.protocol_pool.remove_transactions(vec![hash]);
            }
            Some(store) => {
                let mut pool = store.pool.write();
                let removed = pool.remove_transactions(&[hash]);
                store.listeners.write().on_replaced(&removed, replaced_by);
            }
        }
        self.guard.write().release(&hash);
        ValidityPoolMetrics::record_cross_store_replacement(cross.store != PoolStore::Validity);
    }

    fn stale_classification_error(hash: TxHash) -> PoolError {
        PoolError::other(hash, "EIP-8130 admission classification changed during validation")
    }

    fn ensure_protocol_classification_current(
        &self,
        hash: TxHash,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<()> {
        let current = validated.as_valid_transaction().is_none_or(|transaction| {
            transaction.transaction().limit_class().is_none_or(|class| {
                class.classification_generation
                    == self.validator().validator().limit_class_cache_generation()
            })
        });
        if current { Ok(()) } else { Err(Self::stale_classification_error(hash)) }
    }

    fn pre_admit_protocol_transaction(
        &self,
        hash: TxHash,
        replaced: Option<TxHash>,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<bool> {
        let mut guard = self.guard.write();
        if guard.contains(&hash) {
            return Ok(false);
        }
        let Some(admission) = validated
            .as_valid_transaction()
            .and_then(|transaction| Self::admission_for(transaction.transaction()))
        else {
            return Ok(false);
        };
        if let Some(replaced) = replaced.filter(|replaced| guard.contains(replaced)) {
            // Rejecting here, before reth swaps the transactions, keeps the
            // replaced transaction pooled.
            guard
                .check_replacement(&replaced, &admission)
                .map_err(|rejection| Self::limit_rejection_error(hash, rejection))?;
            return Ok(false);
        }
        if let Err(rejection) = guard.try_admit(admission) {
            return Err(Self::limit_rejection_error(hash, rejection));
        }
        Ok(true)
    }

    fn add_validated_protocol_transaction(
        &self,
        origin: TransactionOrigin,
        hash: TxHash,
        sender: Address,
        nonce: u64,
        validated: TransactionValidationOutcome<T>,
    ) -> PoolResult<AddedTransactionOutcome> {
        self.admit_protocol_transaction(hash, sender, nonce, validated, |validated| {
            let mut outcomes =
                self.protocol_pool.inner().add_transactions(origin, std::iter::once(validated));
            outcomes
                .pop()
                .unwrap_or_else(|| Err(PoolError::other(hash, "inner pool returned no outcome")))
        })
    }

    /// Admits a validated protocol-nonce transaction into the reth pool through
    /// `insert`, under the admission lock. Reserves guard capacity first,
    /// replaces a same-nonce transaction the validity pool holds, and
    /// re-anchors the sender's validity lane to the pending run the newcomer
    /// extends or, as a replacement, cuts short.
    fn admit_protocol_transaction<R>(
        &self,
        hash: TxHash,
        sender: Address,
        nonce: u64,
        validated: TransactionValidationOutcome<T>,
        insert: impl FnOnce(TransactionValidationOutcome<T>) -> PoolResult<R>,
    ) -> PoolResult<R> {
        let _admission_guard = self.protocol_admission_lock.lock();
        // Reject stale validation before reth can replace the currently pooled transaction.
        self.ensure_protocol_classification_current(hash, &validated)?;
        self.ensure_not_published_expired(hash, &validated)?;
        self.ensure_eip8130_fits_block_gas_limit(hash, &validated)?;
        let cross = match validated.as_valid_transaction() {
            Some(transaction) => {
                self.cross_store_replacement(PoolStore::Protocol, transaction.transaction())?
            }
            None => None,
        };
        let replaced = self
            .protocol_replacement_hash(sender, nonce)
            .or_else(|| cross.as_ref().map(|cross| *cross.transaction.hash()));
        // Reserve guard capacity before reth publishes insertion events. A
        // tracked replacement bypasses this check and is reaccounted after the
        // protocol pool atomically accepts the fee bump.
        let pre_admitted = self.pre_admit_protocol_transaction(hash, replaced, &validated)?;
        // Capture the block-expiry bounds before `validated` is consumed below.
        let block_expiry_bound = Self::validity_block_expiry_bound(&validated);
        let flashblock_expiry_bound = Self::validity_flashblock_expiry_bound(&validated);
        let result = match insert(validated) {
            Ok(result) => result,
            Err(error) => {
                if pre_admitted {
                    self.guard.write().release(&hash);
                }
                return Err(error);
            }
        };
        self.gate_protocol_admission(hash, replaced, pre_admitted)?;
        if let Some(cross) = cross {
            self.remove_cross_store_replaced(cross, hash);
        }
        self.register_block_expiry(hash, block_expiry_bound, flashblock_expiry_bound, replaced);
        self.share_validity_lane((sender, U256::ZERO));
        self.sync_validity_lanes([((sender, U256::ZERO), None)]);
        Ok(result)
    }

    fn gate_protocol_admission(
        &self,
        hash: TxHash,
        replaced: Option<TxHash>,
        pre_admitted: bool,
    ) -> PoolResult<()> {
        let Some(transaction) = self.protocol_pool.get(&hash) else {
            let mut guard = self.guard.write();
            if pre_admitted {
                guard.release(&hash);
            } else if let Some(replaced) = replaced {
                guard.release(&replaced);
            }
            return Ok(());
        };
        let mut guard = self.guard.write();
        let replaced_was_tracked = replaced.is_some_and(|hash| guard.release(&hash));
        let Some(admission) = Self::admission_for(&transaction.transaction) else {
            debug_assert!(!pre_admitted, "non-EIP-8130 transaction was pre-admitted");
            if pre_admitted {
                guard.release(&hash);
            }
            return Ok(());
        };
        let current = transaction.transaction.limit_class().is_some_and(|class| {
            class.classification_generation
                == self.validator().validator().limit_class_cache_generation()
        });
        if !current {
            // All generation updates take `protocol_admission_lock`, which is
            // held from the pre-insertion freshness check through this gate.
            // Keep this conservative backstop for future callers that violate
            // that lock discipline rather than admitting stale bookkeeping.
            debug_assert!(current, "classification changed under protocol admission lock");
            if pre_admitted {
                guard.release(&hash);
            }
            drop(guard);
            self.protocol_pool.remove_transactions(vec![hash]);
            return Err(Self::stale_classification_error(hash));
        }
        if replaced_was_tracked {
            guard.insert_forced(admission);
            return Ok(());
        }
        if pre_admitted && !guard.contains(&hash) {
            // A reservation that vanished despite admission serialization was
            // explicitly invalidated or removed. Never recreate it from the
            // now-stale validation snapshot.
            drop(guard);
            self.protocol_pool.remove_transactions(vec![hash]);
            return Err(Self::stale_classification_error(hash));
        }
        if let Err(rejection) = guard.try_admit(admission) {
            drop(guard);
            self.protocol_pool.remove_transactions(vec![hash]);
            return Err(Self::limit_rejection_error(hash, rejection));
        }
        Ok(())
    }

    async fn add_lane_transaction(
        &self,
        store: PoolStore,
        origin: TransactionOrigin,
        transaction: T,
    ) -> PoolResult<AddedTransactionOutcome> {
        let balance_epoch = match self.lane_store(store) {
            Some(lane_store) => lane_store.pool.read().balance_epoch(),
            None => {
                return Err(PoolError::other(
                    *transaction.hash(),
                    "protocol-nonce transaction routed to a lane store",
                ));
            }
        };
        let validated = self.validator().validate_transaction(origin, transaction).await;
        self.add_validated_lane_transaction(store, validated, origin, balance_epoch)
    }

    /// Where a lane transaction entering `store` anchors its lane. Sidecar
    /// lanes sit at the validated channel nonce. Validity lanes additionally
    /// place their executable cursor behind the standard store's pending run,
    /// cut short at a same-slot transaction this insertion replaces.
    fn insert_cursor(
        &self,
        store: PoolStore,
        transaction: &T,
        state_nonce: u64,
        cross: Option<&CrossStoreReplacement<T>>,
    ) -> LaneCursor {
        if store != PoolStore::Validity {
            return LaneCursor::at(state_nonce);
        }
        let Some(lane_id) = Self::lane_id_of(transaction) else {
            return LaneCursor::at(state_nonce);
        };
        let mut next_nonce = self.standard_pending_end(lane_id, state_nonce);
        if let Some(cross) = cross {
            next_nonce = next_nonce.min(cross.transaction.nonce());
        }
        LaneCursor { state_nonce, next_nonce: next_nonce.max(state_nonce) }
    }

    /// Undoes a lane insertion that a post-insert check rejected: removes the
    /// newcomer, re-announces the lane, and restores the transaction it replaced
    /// within the same store.
    #[expect(clippy::too_many_arguments, reason = "borrows the locks the caller already holds")]
    fn roll_back_lane_insert(
        pool: &mut TwoDNoncePool<T>,
        listeners: &mut SidecarListeners<T>,
        guard: &mut MempoolGuard,
        outcome: &InsertOutcome<T>,
        cursor: LaneCursor,
        sender: Address,
        balance: U256,
        balance_epoch: u64,
    ) {
        let removed = pool.remove_transactions(&[outcome.outcome.hash]);
        listeners.on_discarded(&removed);
        listeners.on_lane_moved(&outcome.anchor);
        let Some(replaced) = &outcome.replaced else {
            return;
        };
        let restored = ValidPoolTransaction {
            transaction_id: replaced.transaction_id,
            transaction: replaced.transaction.clone(),
            propagate: replaced.propagate,
            timestamp: replaced.timestamp,
            origin: replaced.origin,
            authority_ids: replaced.authority_ids.clone(),
        };
        if pool.insert_validated_at(restored, cursor).is_err() {
            guard.release(replaced.hash());
            listeners.on_discarded(std::slice::from_ref(replaced));
        }
        pool.record_validated_balance(sender, balance, balance_epoch);
    }

    /// Inserts a validated transaction into the lane store `store`.
    /// `balance_epoch` is that store's balance epoch read before validation
    /// started.
    fn add_validated_lane_transaction(
        &self,
        store: PoolStore,
        validated: TransactionValidationOutcome<T>,
        origin: TransactionOrigin,
        balance_epoch: u64,
    ) -> PoolResult<AddedTransactionOutcome> {
        // Capture the block-expiry bound before `validated` is consumed by the
        // match below, mirroring the protocol admission path. A transaction
        // carrying block-number validity predicates must be recorded in the
        // block-expiry index so it is evicted once its last valid block passes;
        // a finite 2D-nonce-channel transaction has no time-based expiry, so
        // without this it would linger indefinitely.
        let block_expiry_bound = Self::validity_block_expiry_bound(&validated);
        let flashblock_expiry_bound = Self::validity_flashblock_expiry_bound(&validated);
        let _admission_guard = self.protocol_admission_lock.lock();
        if let Some(transaction) = validated.as_valid_transaction() {
            let hash = *transaction.transaction().hash();
            self.ensure_not_published_expired(hash, &validated)?;
            self.ensure_eip8130_fits_block_gas_limit(hash, &validated)?;
        }
        match validated {
            TransactionValidationOutcome::Valid {
                balance,
                transaction,
                propagate,
                state_nonce,
                ..
            } => {
                let validated = Self::validated_pool_transaction(transaction, origin, propagate);
                let hash = *validated.hash();
                let Some(lane_store) = self.lane_store(store) else {
                    return Err(PoolError::other(
                        hash,
                        "protocol-nonce transaction routed to a lane store",
                    ));
                };
                // Look across stores before taking this store's lock: the
                // sidecar is locked ahead of the validity pool everywhere.
                let cross = self.cross_store_replacement(store, &validated.transaction)?;
                let cursor =
                    self.insert_cursor(store, &validated.transaction, state_nonce, cross.as_ref());
                let generation = validated
                    .transaction
                    .limit_class()
                    .map(|class| class.classification_generation);
                let admission = Self::admission_for(&validated.transaction);
                let lane_id = Self::lane_id_of(&validated.transaction);
                let sender = validated.sender();
                // Keep the lock order consistent everywhere: pool before listeners.
                let mut pool = lane_store.pool.write();
                let mut listeners = lane_store.listeners.write();
                let mut outcome = pool.insert_validated_at(validated, cursor)?;
                pool.record_validated_balance(sender, balance, balance_epoch);
                // The pool serializes lane replacement. Never acquire it while holding guard.
                let mut guard = self.guard.write();
                // Transactions below the anchored lane cursor are gone whether or
                // not this insertion is kept.
                for transaction in &outcome.anchor.discarded {
                    guard.release(transaction.hash());
                }
                listeners.on_discarded(&outcome.anchor.discarded);
                let current = generation.is_none_or(|generation| {
                    generation == self.validator().validator().limit_class_cache_generation()
                });
                let replaced_hash = outcome
                    .replaced
                    .as_ref()
                    .map(|replaced| *replaced.hash())
                    .or_else(|| cross.as_ref().map(|cross| *cross.transaction.hash()));
                // A tracked replacement is reaccounted by force once the lane
                // accepted the fee bump; an untracked one admits the newcomer
                // under the regular limits.
                let tracked_replaced = replaced_hash.filter(|replaced| guard.contains(replaced));
                let rejection = if !current {
                    Some(Self::stale_classification_error(hash))
                } else if let (Some(replaced), Some(admission)) = (tracked_replaced, &admission) {
                    guard
                        .check_replacement(&replaced, admission)
                        .err()
                        .map(|rejection| Self::limit_rejection_error(hash, rejection))
                } else {
                    None
                };
                if let Some(error) = rejection {
                    Self::roll_back_lane_insert(
                        &mut pool,
                        &mut listeners,
                        &mut guard,
                        &outcome,
                        cursor,
                        sender,
                        balance,
                        balance_epoch,
                    );
                    return Err(error);
                }
                // An incumbent from another store stays pooled there until the
                // newcomer survives eviction below, so its reservation is only
                // released then; until that point both are reserved, which
                // `insert_forced` allows.
                let cross_store_incumbent = tracked_replaced.filter(|_| outcome.replaced.is_none());
                match (tracked_replaced, admission) {
                    (Some(replaced), Some(admission)) => {
                        if cross_store_incumbent.is_none() {
                            guard.release(&replaced);
                        }
                        guard.insert_forced(admission);
                    }
                    (Some(replaced), None) => {
                        if cross_store_incumbent.is_none() {
                            guard.release(&replaced);
                        }
                    }
                    (None, Some(admission)) => {
                        if let Err(rejection) = guard.try_admit(admission) {
                            Self::roll_back_lane_insert(
                                &mut pool,
                                &mut listeners,
                                &mut guard,
                                &outcome,
                                cursor,
                                sender,
                                balance,
                                balance_epoch,
                            );
                            return Err(Self::limit_rejection_error(hash, rejection));
                        }
                    }
                    (None, None) => {}
                }
                let mut evicted = pool.evict_over_capacity();
                for transaction in &evicted {
                    guard.release(transaction.hash());
                }
                let inserted_hash = outcome.outcome.hash;
                let discarded_on_insert =
                    evicted.iter().any(|transaction| *transaction.hash() == inserted_hash);
                if !discarded_on_insert && let Some(incumbent) = cross_store_incumbent {
                    guard.release(&incumbent);
                }
                drop(guard);
                let survived = |moved: &Arc<ValidPoolTransaction<T>>| {
                    !evicted.iter().any(|transaction| transaction.hash() == moved.hash())
                };
                outcome.anchor.promoted.retain(survived);
                outcome.anchor.queued.retain(survived);
                outcome.promoted.retain(survived);
                if store == PoolStore::Validity {
                    ValidityPoolMetrics::record_evictions(evicted.len());
                }
                if discarded_on_insert {
                    evicted.retain(|transaction| *transaction.hash() != inserted_hash);
                    evicted.extend(outcome.replaced);
                    listeners.on_discarded(&evicted);
                    listeners.on_lane_moved(&outcome.anchor);
                    return Err(PoolError::new(inserted_hash, PoolErrorKind::DiscardedOnInsert));
                }
                listeners.on_discarded(&evicted);
                listeners.on_inserted(&pool, &outcome);
                if store == PoolStore::Validity {
                    ValidityPoolMetrics::record_admission(replaced_hash.is_some());
                }
                // Release this store's locks before touching any other store or
                // the block-expiry index.
                drop(listeners);
                drop(pool);
                if let Some(cross) = cross {
                    self.remove_cross_store_replaced(cross, inserted_hash);
                }
                self.register_block_expiry(
                    inserted_hash,
                    block_expiry_bound,
                    flashblock_expiry_bound,
                    replaced_hash,
                );
                match store {
                    PoolStore::Sidecar => {
                        if let Some(lane_id) = lane_id {
                            self.share_validity_lane(lane_id);
                        }
                    }
                    PoolStore::Validity => {
                        if let Some(lane_id) =
                            lane_id.filter(|lane_id| self.standard_store_holds_lane(*lane_id))
                        {
                            self.validity.pool.write().mark_shared_lane(lane_id);
                        }
                    }
                    PoolStore::Protocol => {}
                }
                if store == PoolStore::Sidecar {
                    // Validity lanes on the channels this insertion extended or
                    // the eviction shortened follow the sidecar's pending run.
                    self.sync_validity_lanes(
                        lane_id
                            .into_iter()
                            .chain(
                                evicted
                                    .iter()
                                    .filter_map(|evicted| Self::lane_id_of(&evicted.transaction)),
                            )
                            .map(|lane_id| (lane_id, None)),
                    );
                }
                Ok(outcome.outcome)
            }
            TransactionValidationOutcome::Invalid(transaction, error) => {
                Err(PoolError::new(*transaction.hash(), PoolErrorKind::InvalidTransaction(error)))
            }
            TransactionValidationOutcome::Error(hash, error) => {
                Err(PoolError::other(hash, error.to_string()))
            }
        }
    }

    /// Wraps a validated lane transaction. Its sender and authority ids are
    /// placeholders until [`TwoDNoncePool::insert_validated`] assigns them.
    fn validated_pool_transaction(
        transaction: reth_transaction_pool::validate::ValidTransaction<T>,
        origin: TransactionOrigin,
        propagate: bool,
    ) -> ValidPoolTransaction<T> {
        let transaction = transaction.into_transaction();
        ValidPoolTransaction {
            transaction_id: reth_transaction_pool::identifier::TransactionId::new(
                0u64.into(),
                transaction.nonce(),
            ),
            transaction,
            propagate,
            timestamp: std::time::Instant::now(),
            origin,
            authority_ids: None,
        }
    }

    fn merged_pending_listener(&self, kind: TransactionListenerKind) -> mpsc::Receiver<TxHash> {
        let protocol = self.protocol_pool.pending_transactions_listener_for(kind);
        let sidecar = self.sidecar.listeners.write().subscribe_pending(kind);
        let validity = self.validity.listeners.write().subscribe_pending(kind);
        merge_receivers(protocol, merge_receivers(sidecar, validity))
    }

    fn merged_new_transactions_listener(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        let protocol = self.protocol_pool.new_transactions_listener_for(kind);
        let sidecar = self.sidecar.listeners.write().subscribe_new_transactions(kind);
        let validity = self.validity.listeners.write().subscribe_new_transactions(kind);
        merge_receivers(protocol, merge_receivers(sidecar, validity))
    }

    fn merged_all_transactions_listener(&self) -> AllTransactionsEvents<T> {
        let protocol = self.protocol_pool.all_transactions_event_listener();
        let sidecar = self.sidecar.listeners.write().subscribe_all();
        let validity = self.validity.listeners.write().subscribe_all();
        let mut merged = futures::stream::select_all([protocol, sidecar, validity]);
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        spawn(async move {
            while let Some(event) = merged.next().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        AllTransactionsEvents::new(rx)
    }

    fn merged_best_transactions(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> MergeBestTransactions<T, O> {
        let base_fee = attributes.basefee;
        // The lane bookkeeping the merge relies on (anchors, shared lanes) and
        // the lane stores' iterators are snapshotted under one pair of read
        // guards, taken in lock order (sidecar before validity), so an admission
        // cannot land between them and hand the merge a head whose lane it does
        // not know is ahead of state. Only reth's snapshot is taken separately,
        // and a stale one leaves a validity head waiting, never released early.
        // The bookkeeping comes from the validity pool's incremental indexes,
        // so this costs the lanes that straddle stores, not the pool sizes.
        let (sidecar, validity, anchors, bridged) = {
            let sidecar = self.sidecar.pool.read();
            let validity = self.validity.pool.read();
            (
                sidecar.best_transactions(self.ordering.clone(), base_fee),
                validity.best_transactions(self.ordering.clone(), base_fee),
                validity.lanes_ahead_of_state().clone(),
                self.bridged_descendants(&sidecar, &validity),
            )
        };
        // Reth hands back a boxed iterator of a private type, so it is boxed
        // once more as a `MergeSource`: one allocation per build.
        let protocol = self.protocol_pool.best_transactions_with_attributes(attributes);
        MergeBestTransactions::new(
            Box::new(protocol),
            Box::new(sidecar),
            Box::new(validity),
            self.ordering.clone(),
            base_fee,
        )
        .with_external_anchors(anchors)
        .with_bridged_descendants(bridged)
    }

    /// For each validity lane that shares its nonce space with a standard
    /// store, the consecutive run that store holds from where the validity
    /// lane's pending run ends. The store queues those transactions behind the
    /// gap the validity run fills, so only the merge can release them, after
    /// that run.
    fn bridged_descendants(
        &self,
        sidecar: &TwoDNoncePool<T>,
        validity: &TwoDNoncePool<T>,
    ) -> BridgedDescendants<T> {
        let mut bridged = BridgedDescendants::default();
        for (lane_id, end) in validity.shared_lanes_pending_end() {
            let run: VecDeque<_> = (end..)
                .map_while(|nonce| self.standard_transaction(sidecar, lane_id, nonce))
                .collect();
            if !run.is_empty() {
                bridged.insert(lane_id, run);
            }
        }
        bridged
    }

    /// The transaction the standard store owning `lane_id` holds at `nonce`,
    /// pending or queued.
    fn standard_transaction(
        &self,
        sidecar: &TwoDNoncePool<T>,
        lane_id: LaneId,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let (sender, nonce_key) = lane_id;
        if nonce_key.is_zero() {
            self.protocol_pool.get_transaction_by_sender_and_nonce(sender, nonce)
        } else {
            sidecar.lane_transaction(&lane_id, nonce)
        }
    }

    /// Records that `lane_id` now has transactions in both the validity pool
    /// and its standard store, so builds look for standard descendants behind
    /// the validity run. Called from both stores' admission paths, so the two
    /// meet whichever arrives second; `sync_validity_lanes` clears the flag
    /// once the standard store has emptied the lane again.
    fn share_validity_lane(&self, lane_id: LaneId) {
        if self.validity.pool.read().lane_cursor(&lane_id).is_none() {
            return;
        }
        self.validity.pool.write().mark_shared_lane(lane_id);
    }

    /// Whether the standard store owning `lane_id` holds any transaction in it.
    fn standard_store_holds_lane(&self, lane_id: LaneId) -> bool {
        let (sender, nonce_key) = lane_id;
        if nonce_key.is_zero() {
            self.protocol_pool.get_highest_transaction_by_sender(sender).is_some()
        } else {
            self.sidecar.pool.read().lane_cursor(&lane_id).is_some()
        }
    }
}

impl<Client, S, Evm, T, O> StateDiffInvalidation for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn invalidate_from_state_diff(&self, diffs: &[AccountStateDiff]) -> usize {
        self.apply_state_diff(diffs).len()
    }

    fn invalidate_all_tracked(&self, cause: InvalidationCause) -> usize {
        self.invalidate_all_tracked_transactions(cause).len()
    }
}

impl<Client, S, Evm, T, O> FlashblockExpiry for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn publish_and_expire<R, E>(
        &self,
        block_number: u64,
        flashblock_index: u64,
        publish: impl FnOnce() -> Result<R, E>,
    ) -> Result<(R, usize), E> {
        let _admission_guard = self.protocol_admission_lock.lock();
        let result = publish()?;
        let expired = self.expire_published_locked(block_number, flashblock_index);
        Ok((result, expired))
    }

    fn expire_published(&self, block_number: u64, flashblock_index: u64) -> usize {
        let _admission_guard = self.protocol_admission_lock.lock();
        self.expire_published_locked(block_number, flashblock_index)
    }
}

impl<Client, S, Evm, T, O> TransactionPool for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Transaction = T;

    fn pool_size(&self) -> PoolSize {
        let mut size = self.protocol_pool.pool_size();
        for store in self.lane_stores() {
            let store_size = store.pool.read().pool_size();
            size.pending += store_size.pending;
            size.pending_size += store_size.pending_size;
            size.queued += store_size.queued;
            size.queued_size += store_size.queued_size;
            size.total += store_size.total;
        }
        size
    }

    fn block_info(&self) -> BlockInfo {
        self.protocol_pool.block_info()
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        let store = PoolStore::of(&transaction);
        let hash = *transaction.hash();
        let Some(lane_store) = self.lane_store(store) else {
            let sender = transaction.sender();
            let nonce = transaction.nonce();
            let validated = self.validator().validate_transaction(origin, transaction).await;
            return self.admit_protocol_transaction(hash, sender, nonce, validated, |validated| {
                self.protocol_pool.inner().add_transaction_and_subscribe(origin, validated)
            });
        };

        let (events, listener) = lane_store.listeners.write().subscribe_hash(hash);
        if let Err(error) = self.add_lane_transaction(store, origin, transaction).await {
            lane_store.listeners.write().unsubscribe_hash_listener(&hash, &listener);
            return Err(error);
        }
        Ok(events)
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        match PoolStore::of(&transaction) {
            PoolStore::Protocol => {
                let hash = *transaction.hash();
                let sender = transaction.sender();
                let nonce = transaction.nonce();
                let validated = self.validator().validate_transaction(origin, transaction).await;
                self.add_validated_protocol_transaction(origin, hash, sender, nonce, validated)
            }
            store => self.add_lane_transaction(store, origin, transaction).await,
        }
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        self.add_transactions_with_origins(
            transactions.into_iter().map(|transaction| (origin, transaction)).collect(),
        )
        .await
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        if transactions.iter().any(|(_, transaction)| PoolStore::of(transaction).is_lane_store()) {
            let mut results = Vec::with_capacity(transactions.len());
            for (origin, transaction) in transactions {
                results.push(self.add_transaction(origin, transaction).await);
            }
            return results;
        }
        // Protocol validation reads canonical state, never pool contents. Every
        // transaction is validated against one snapshot taken when the batch
        // job starts and admitted in order only after the whole batch is
        // validated, so outcomes match per-transaction adds unless the
        // canonical head or the pool changes concurrently in that window.
        let keys: Vec<_> = transactions
            .iter()
            .map(|(origin, tx)| (*origin, *tx.hash(), tx.sender(), tx.nonce()))
            .collect();
        let validated = self.validator().validate_transactions(transactions).await;
        keys.into_iter()
            .zip(validated)
            .map(|((origin, hash, sender, nonce), validated)| {
                self.add_validated_protocol_transaction(origin, hash, sender, nonce, validated)
            })
            .collect()
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        self.protocol_pool.transaction_event_listener(tx_hash).or_else(|| {
            self.lane_stores().into_iter().find_map(|store| {
                store
                    .pool
                    .read()
                    .contains(&tx_hash)
                    .then(|| store.listeners.write().subscribe_hash(tx_hash).0)
            })
        })
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        self.merged_all_transactions_listener()
    }

    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash> {
        self.merged_pending_listener(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> mpsc::Receiver<NewBlobSidecar> {
        self.protocol_pool.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<Self::Transaction>> {
        self.merged_new_transactions_listener(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.pooled_transaction_hashes();
        for store in self.lane_stores() {
            hashes.extend(
                store
                    .pool
                    .read()
                    .all_transactions()
                    .into_iter()
                    .filter(|transaction| transaction.propagate)
                    .map(|transaction| *transaction.hash()),
            );
        }
        hashes
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.pooled_transaction_hashes_max(max);
        for store in self.lane_stores() {
            if hashes.len() >= max {
                break;
            }
            let remaining = max - hashes.len();
            hashes.extend(
                store
                    .pool
                    .read()
                    .all_transactions_iter()
                    .filter(|transaction| transaction.propagate)
                    .map(|transaction| *transaction.hash())
                    .take(remaining),
            );
        }
        hashes
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions();
        for store in self.lane_stores() {
            transactions.extend(
                store
                    .pool
                    .read()
                    .all_transactions()
                    .into_iter()
                    .filter(|transaction| transaction.propagate),
            );
        }
        transactions
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions_max(max);
        for store in self.lane_stores() {
            if transactions.len() >= max {
                break;
            }
            let remaining = max - transactions.len();
            transactions.extend(
                store
                    .pool
                    .read()
                    .all_transactions_iter()
                    .filter(|transaction| transaction.propagate)
                    .take(remaining)
                    .cloned(),
            );
        }
        transactions
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        let mut pooled = Vec::new();
        self.append_pooled_transaction_elements(&tx_hashes, limit, &mut pooled);
        pooled
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<Self::Transaction as PoolTransaction>::Pooled>,
    ) {
        let mut current_size = 0;
        for hash in tx_hashes {
            let Some(transaction) = self.get(hash) else {
                continue;
            };
            let Some((pooled, encoded_length)) = pooled_element(&transaction) else {
                continue;
            };
            current_size += encoded_length;
            if limit.exceeds(current_size) {
                break;
            }
            out.push(pooled);
        }
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<<Self::Transaction as PoolTransaction>::Pooled>> {
        self.protocol_pool.get_pooled_transaction_element(tx_hash).or_else(|| {
            self.lane_stores().into_iter().find_map(|store| {
                store
                    .pool
                    .read()
                    .get(&tx_hash)
                    .and_then(|transaction| transaction.transaction.clone().try_into_pooled().ok())
            })
        })
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        let block_info = self.protocol_pool.block_info();
        let best_transactions_attributes = BestTransactionsAttributes::new(
            block_info.pending_basefee,
            block_info.pending_blob_fee.map(|fee| u64::try_from(fee).unwrap_or(u64::MAX)),
        );
        Box::new(self.merged_best_transactions(best_transactions_attributes))
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        Box::new(self.merged_best_transactions(best_transactions_attributes))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions();
        for store in self.lane_stores() {
            transactions.extend(store.pool.read().pending_transactions());
        }
        transactions
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // Channelized nonce sequences and nonce-free replay ids live in separate
        // namespaces from account nonces, so this sender+nonce lookup covers the
        // protocol pool and the validity pool's protocol-nonce lanes only.
        self.protocol_pool.get_pending_transaction_by_sender_and_nonce(sender, nonce).or_else(
            || self.validity.pool.read().lane_pending_transaction(&(sender, U256::ZERO), nonce),
        )
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions_max(max);
        for store in self.lane_stores() {
            if transactions.len() >= max {
                break;
            }
            let remaining = max - transactions.len();
            transactions
                .extend(store.pool.read().pending_transactions_iter().take(remaining).cloned());
        }
        transactions
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.queued_transactions();
        for store in self.lane_stores() {
            transactions.extend(store.pool.read().queued_transactions());
        }
        transactions
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (mut pending, mut queued) = self.protocol_pool.pending_and_queued_txn_count();
        for store in self.lane_stores() {
            let (store_pending, store_queued) = store.pool.read().pending_and_queued_txn_count();
            pending += store_pending;
            queued += store_queued;
        }
        (pending, queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        let mut transactions = self.protocol_pool.all_transactions();
        for store in self.lane_stores() {
            let pool = store.pool.read();
            transactions.pending.extend(pool.pending_transactions());
            transactions.queued.extend(pool.queued_transactions());
        }
        transactions
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.all_transaction_hashes();
        for store in self.lane_stores() {
            hashes.extend(store.pool.read().all_hashes());
        }
        hashes
    }

    fn all_transactions_by_sender(
        &self,
        sender: Address,
    ) -> AllPoolTransactions<Self::Transaction> {
        let mut transactions = self.protocol_pool.all_transactions_by_sender(sender);
        for store in self.lane_stores() {
            let pool = store.pool.read();
            transactions.pending.extend(pool.pending_transactions_by_sender(sender));
            transactions.queued.extend(pool.queued_transactions_by_sender(sender));
        }
        transactions
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // Removals re-anchor validity lanes from the standard stores' state, so
        // they are serialized with admission like every other re-anchoring path.
        let _admission_guard = self.protocol_admission_lock.lock();
        self.remove_from_stores(hashes, RemoveMode::Exact)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let _admission_guard = self.protocol_admission_lock.lock();
        self.remove_from_stores(hashes, RemoveMode::WithDescendants)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let _admission_guard = self.protocol_admission_lock.lock();
        let mut removed = self.protocol_pool.remove_transactions_by_sender(sender);
        for store in self.lane_stores() {
            let mut pool = store.pool.write();
            let store_removed = pool.remove_transactions_by_sender(sender);
            if !store_removed.is_empty() {
                store.listeners.write().on_discarded(&store_removed);
            }
            removed.extend(store_removed);
        }
        // The sender's validity lanes went with the rest, so nothing is left to re-anchor.
        self.release_from_guard(&removed);
        removed
    }

    fn prune_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let _admission_guard = self.protocol_admission_lock.lock();
        self.remove_from_stores(hashes, RemoveMode::Mined)
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.protocol_pool.retain_unknown(announcement);
        if announcement.is_empty() {
            return;
        }

        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        announcement.retain_by_hash(|hash| !sidecar.contains(hash) && !validity.contains(hash));
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        announcement.retain_by_hash(|hash| {
            self.protocol_pool.get(hash).is_some()
                || sidecar.contains(hash)
                || validity.contains(hash)
        });
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get(tx_hash).or_else(|| {
            self.lane_stores().into_iter().find_map(|store| store.pool.read().get(tx_hash))
        })
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        txs.into_iter()
            .filter_map(|tx| {
                self.protocol_pool
                    .get(&tx)
                    .or_else(|| sidecar.get(&tx))
                    .or_else(|| validity.get(&tx))
            })
            .collect()
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        let sidecar = self.sidecar.pool.read();
        let validity = self.validity.pool.read();
        let protocol_txs = PropagatedTransactions(
            txs.0
                .into_iter()
                .filter(|(hash, _)| !sidecar.contains(hash) && !validity.contains(hash))
                .collect(),
        );
        drop(validity);
        drop(sidecar);
        self.protocol_pool.on_propagated(protocol_txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_sender(sender);
        for store in self.lane_stores() {
            transactions.extend(store.pool.read().transactions_by_sender(sender));
        }
        transactions
    }

    fn get_pending_transactions_with_predicate(
        &self,
        mut predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions =
            self.protocol_pool.get_pending_transactions_with_predicate(&mut predicate);
        for store in self.lane_stores() {
            transactions.extend(
                store
                    .pool
                    .read()
                    .pending_transactions()
                    .into_iter()
                    .filter(|transaction| predicate(transaction)),
            );
        }
        transactions
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_pending_transactions_by_sender(sender);
        for store in self.lane_stores() {
            transactions.extend(store.pool.read().pending_transactions_by_sender(sender));
        }
        transactions
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_queued_transactions_by_sender(sender);
        for store in self.lane_stores() {
            transactions.extend(store.pool.read().queued_transactions_by_sender(sender));
        }
        transactions
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let protocol = self.protocol_pool.get_highest_transaction_by_sender(sender);
        let validity = self.validity.pool.read().lane_highest_transaction(&(sender, U256::ZERO));
        match (protocol, validity) {
            (Some(protocol), Some(validity)) => {
                Some(if validity.nonce() > protocol.nonce() { validity } else { protocol })
            }
            (protocol, validity) => protocol.or(validity),
        }
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // The run may alternate between the protocol pool and the validity
        // pool's protocol-nonce lane, so follow it across both until neither
        // holds the next nonce.
        let validity = self.validity.pool.read();
        let lane_id = (sender, U256::ZERO);
        let mut next = on_chain_nonce;
        let mut last = None;
        loop {
            let mut advanced = false;
            if let Some(transaction) =
                self.protocol_pool.get_highest_consecutive_transaction_by_sender(sender, next)
                && transaction.nonce() >= next
            {
                next = transaction.nonce().checked_add(1)?;
                last = Some(transaction);
                advanced = true;
            }
            if let Some(transaction) = validity.highest_consecutive_from(&lane_id, next) {
                next = transaction.nonce().checked_add(1)?;
                last = Some(transaction);
                advanced = true;
            }
            if !advanced {
                return last;
            }
        }
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool
            .get_transaction_by_sender_and_nonce(sender, nonce)
            .or_else(|| self.validity.pool.read().lane_transaction(&(sender, U256::ZERO), nonce))
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_origin(origin);
        for store in self.lane_stores() {
            transactions.extend(
                store
                    .pool
                    .read()
                    .all_transactions()
                    .into_iter()
                    .filter(|transaction| transaction.origin == origin),
            );
        }
        transactions
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_pending_transactions_by_origin(origin);
        for store in self.lane_stores() {
            transactions.extend(
                store
                    .pool
                    .read()
                    .pending_transactions()
                    .into_iter()
                    .filter(|transaction| transaction.origin == origin),
            );
        }
        transactions
    }

    fn unique_senders(&self) -> AddressSet {
        let mut senders = self.protocol_pool.unique_senders();
        for store in self.lane_stores() {
            senders.extend(store.pool.read().unique_senders());
        }
        senders
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.protocol_pool.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        self.protocol_pool.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.protocol_pool.get_all_blobs_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v3(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        cell_mask: BlobCellMask,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v4(versioned_hashes, cell_mask)
    }

    fn has_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<bool>, BlobStoreError> {
        self.protocol_pool.has_blobs_for_versioned_hashes(versioned_hashes)
    }

    fn blob_store(&self) -> Box<dyn BlobStore> {
        Box::new(self.protocol_pool.blob_store().clone())
    }
}

impl<Client, S, Evm, T, O> ParkableTransactionPool for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> Box<dyn ParkableBestTransactions<Self::Transaction>> {
        let base_fee = attributes.basefee;
        let merged = self.merged_best_transactions(attributes);
        Box::new(ParkedBestTransactions::new(merged, self.ordering.clone(), base_fee))
    }
}

impl<Client, S, Evm, T, O> TransactionPoolExt for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Block = <TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>> as TransactionValidator>::Block;

    fn set_block_info(&self, info: BlockInfo) {
        self.protocol_pool.set_block_info(info)
    }

    fn on_canonical_state_change(
        &self,
        update: reth_transaction_pool::CanonicalStateUpdate<'_, Self::Block>,
    ) {
        let block_hash = update.hash();
        let now = update.timestamp();
        let block_number = update.number();
        let mined_transactions = update.mined_transactions.clone();
        let changed_nonces: Vec<(Address, u64)> = update
            .changed_accounts
            .iter()
            .map(|account| (account.address, account.nonce))
            .collect();
        // Free mined capacity atomically with admission before the heavier pool
        // maintenance. The transaction is already canonical, so it no longer
        // consumes an admission slot even until its pool entry is pruned below.
        {
            let _admission_guard = self.protocol_admission_lock.lock();
            if update.update_kind == PoolUpdateKind::Reorg {
                // The new branch can rebuild an earlier block or restart its flashblock indices.
                *self.published_flashblock.write() = None;
            }
            let mut guard = self.guard.write();
            for hash in &mined_transactions {
                guard.release(hash);
            }
            let mut block_expiry = self.block_expiry.write();
            for hash in &mined_transactions {
                block_expiry.remove(hash);
            }
        }
        self.protocol_pool.on_canonical_state_change(update);
        // `protocol_pool.on_canonical_state_change` has already advanced the
        // validator's head clock to this block.
        let now_ms = self.validator().validator().block_timestamp_ms();
        let mut touched_lanes = Vec::new();
        for store in self.lane_stores() {
            let mut pool = store.pool.write();
            let pruned = pool.prune_mined(&mined_transactions);
            let expired = pool.remove_expired_nonce_free(now_ms);
            let mut listeners = store.listeners.write();
            if !pruned.removed.is_empty() {
                listeners.on_mined(&pruned.removed, block_hash);
            }
            listeners.on_lane_update(&pruned.lanes);
            if !expired.is_empty() {
                listeners.on_discarded(&expired);
            }
            // Lane-store maintenance lock order is pool -> listeners -> guard.
            let mut guard = self.guard.write();
            for transaction in pruned.removed.iter().chain(&pruned.lanes.discarded).chain(&expired)
            {
                guard.release(transaction.hash());
            }
            // Mined lane transactions moved their channel's canonical nonce past
            // them; a validity lane on the same channel follows that move.
            touched_lanes.extend(pruned.removed.iter().filter_map(|transaction| {
                Self::lane_id_of(&transaction.transaction)
                    .map(|lane_id| (lane_id, Some(transaction.nonce().saturating_add(1))))
            }));
        }
        {
            // Re-anchor protocol-nonce validity lanes to the new account nonces,
            // and re-check every lane that relied on a standard pending run the
            // protocol pool may have reshuffled on its own.
            let _admission_guard = self.protocol_admission_lock.lock();
            touched_lanes.extend(
                changed_nonces
                    .into_iter()
                    .map(|(address, nonce)| ((address, U256::ZERO), Some(nonce))),
            );
            touched_lanes.extend(
                self.validity.pool.read().lanes_ahead_of_state().keys().map(|lane| (*lane, None)),
            );
            self.sync_validity_lanes(touched_lanes);
        }
        self.expire_due_buckets(now);
        self.expire_by_block(block_number);
        self.reconcile_guard();
        GuardMetrics::tracked().set(self.guard.read().len() as f64);
        let (validity_transactions, validity_bytes) = self.validity_pool_occupancy();
        ValidityPoolMetrics::record_occupancy(validity_transactions, validity_bytes);
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        // Serialize balance invalidation with protocol pre-admission so a
        // dropped reservation cannot be recreated from an older validation
        // snapshot after reth publishes the transaction.
        let _admission_guard = self.protocol_admission_lock.lock();
        for store in self.lane_stores() {
            store.pool.write().apply_balance_updates(
                accounts.iter().map(|account| (account.address, account.balance)),
            );
        }
        let dropped = {
            let mut guard = self.guard.write();
            let mut dropped = Vec::new();
            for account in &accounts {
                dropped.extend(guard.on_balance_changed(account.address, account.balance));
            }
            dropped
        };
        let invalidated = self.remove_from_stores(dropped, RemoveMode::Exact);
        GuardMetrics::record_balance_update_invalidations(invalidated.len());
        if !invalidated.is_empty() {
            debug!(
                count = invalidated.len(),
                "EIP-8130 transactions invalidated by balance change"
            );
        }
        let discard_candidates: Vec<_> = accounts
            .iter()
            .flat_map(|account| self.protocol_pool.get_transactions_by_sender(account.address))
            .map(|transaction| *transaction.hash())
            .collect();
        let validity_lanes: Vec<_> = accounts
            .iter()
            .map(|account| ((account.address, U256::ZERO), Some(account.nonce)))
            .collect();
        self.protocol_pool.update_accounts(accounts);
        let discarded: Vec<_> = discard_candidates
            .into_iter()
            .filter(|hash| self.protocol_pool.get(hash).is_none())
            .collect();
        let mut guard = self.guard.write();
        for hash in discarded {
            guard.release(&hash);
        }
        drop(guard);
        // The protocol pool re-sorted these senders' runs; validity lanes behind them follow.
        self.sync_validity_lanes(validity_lanes);
    }

    fn delete_blob(&self, tx: B256) {
        self.protocol_pool.delete_blob(tx)
    }

    fn delete_blobs(&self, txs: Vec<B256>) {
        self.protocol_pool.delete_blobs(txs)
    }

    fn cleanup_blobs(&self) {
        self.protocol_pool.cleanup_blobs()
    }
}

#[derive(Debug)]
/// Event listeners for transactions in a lane store.
///
/// Bounded listeners follow reth's listener semantics: a listener whose channel is full misses
/// that event but stays subscribed, and is removed only once its receiver is dropped.
struct SidecarListeners<T: BasePooledTx> {
    by_hash: HashMap<TxHash, Vec<mpsc::UnboundedSender<TransactionEvent>>>,
    all_events: Vec<mpsc::Sender<FullTransactionEvent<T>>>,
    pending_all: Vec<mpsc::Sender<TxHash>>,
    pending_propagate: Vec<mpsc::Sender<TxHash>>,
    new_all: Vec<mpsc::Sender<NewTransactionEvent<T>>>,
    new_propagate: Vec<mpsc::Sender<NewTransactionEvent<T>>>,
}

impl<T: BasePooledTx> Default for SidecarListeners<T> {
    fn default() -> Self {
        Self {
            by_hash: HashMap::new(),
            all_events: Vec::new(),
            pending_all: Vec::new(),
            pending_propagate: Vec::new(),
            new_all: Vec::new(),
            new_propagate: Vec::new(),
        }
    }
}

impl<T: BasePooledTx> SidecarListeners<T> {
    fn subscribe_hash(
        &mut self,
        tx_hash: TxHash,
    ) -> (TransactionEvents, mpsc::UnboundedSender<TransactionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.by_hash.entry(tx_hash).or_default().push(tx.clone());
        (TransactionEvents::new(tx_hash, rx), tx)
    }

    fn unsubscribe_hash_listener(
        &mut self,
        tx_hash: &TxHash,
        listener: &mpsc::UnboundedSender<TransactionEvent>,
    ) {
        let Some(listeners) = self.by_hash.get_mut(tx_hash) else {
            return;
        };
        listeners.retain(|candidate| !candidate.same_channel(listener));
        if listeners.is_empty() {
            self.by_hash.remove(tx_hash);
        }
    }

    fn subscribe_all(&mut self) -> AllTransactionsEvents<T> {
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        self.all_events.push(tx);
        AllTransactionsEvents::new(rx)
    }

    fn subscribe_pending(&mut self, kind: TransactionListenerKind) -> mpsc::Receiver<TxHash> {
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        if kind.is_propagate_only() {
            self.pending_propagate.push(tx);
        } else {
            self.pending_all.push(tx);
        }
        rx
    }

    fn subscribe_new_transactions(
        &mut self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        if kind.is_propagate_only() {
            self.new_propagate.push(tx);
        } else {
            self.new_all.push(tx);
        }
        rx
    }

    fn on_inserted(&mut self, pool: &TwoDNoncePool<T>, outcome: &InsertOutcome<T>) {
        let hash = outcome.outcome.hash;
        let Some(transaction) = pool.get(&hash) else {
            return;
        };

        if let Some(replaced) = &outcome.replaced {
            self.on_replaced(std::slice::from_ref(replaced), hash);
        }

        match &outcome.outcome.state {
            AddedTransactionState::Pending => {
                self.broadcast_pending_transaction(&transaction);
            }
            AddedTransactionState::Queued(reason) => {
                self.broadcast_hash_event(&hash, TransactionEvent::Queued);
                self.broadcast_all(FullTransactionEvent::Queued(hash, Some(reason.clone())));
                self.broadcast_new(NewTransactionEvent { subpool: SubPool::Queued, transaction });
            }
        }

        self.on_lane_moved(&outcome.anchor);
        for promoted in &outcome.promoted {
            self.broadcast_pending_transaction(promoted);
        }
    }

    /// Publishes that `replaced_by` superseded each of `transactions`.
    fn on_replaced(&mut self, transactions: &[Arc<ValidPoolTransaction<T>>], replaced_by: TxHash) {
        for transaction in transactions {
            self.broadcast_hash_event(transaction.hash(), TransactionEvent::Replaced(replaced_by));
            self.broadcast_all(FullTransactionEvent::Replaced {
                transaction: Arc::clone(transaction),
                replaced_by,
            });
        }
    }

    /// Publishes a lane cursor move: removed transactions are discarded, and
    /// the rest of the lane is re-announced where its subpool changed.
    fn on_lane_update(&mut self, update: &LaneUpdate<T>) {
        self.on_discarded(&update.discarded);
        self.on_lane_moved(update);
    }

    fn on_lane_moved(&mut self, update: &LaneUpdate<T>) {
        for promoted in &update.promoted {
            self.broadcast_pending_transaction(promoted);
        }
        for queued in &update.queued {
            let hash = *queued.hash();
            self.broadcast_hash_event(&hash, TransactionEvent::Queued);
            self.broadcast_all(FullTransactionEvent::Queued(hash, Some(QueuedReason::NonceGap)));
        }
    }

    fn on_mined(&mut self, transactions: &[Arc<ValidPoolTransaction<T>>], block_hash: B256) {
        for transaction in transactions {
            let hash = *transaction.hash();
            self.broadcast_hash_event(&hash, TransactionEvent::Mined(block_hash));
            self.broadcast_all(FullTransactionEvent::Mined { tx_hash: hash, block_hash });
        }
    }

    fn on_discarded(&mut self, transactions: &[Arc<ValidPoolTransaction<T>>]) {
        for transaction in transactions {
            let hash = *transaction.hash();
            self.broadcast_hash_event(&hash, TransactionEvent::Discarded);
            self.broadcast_all(FullTransactionEvent::Discarded(hash));
        }
    }

    fn broadcast_hash_event(&mut self, tx_hash: &TxHash, event: TransactionEvent) {
        let Some(listeners) = self.by_hash.get_mut(tx_hash) else {
            return;
        };
        listeners.retain(|listener| listener.send(event.clone()).is_ok() && !event.is_final());
        if listeners.is_empty() {
            self.by_hash.remove(tx_hash);
        }
    }

    fn broadcast_all(&mut self, event: FullTransactionEvent<T>) {
        Self::send_to_open(&mut self.all_events, &event);
    }

    fn broadcast_pending_transaction(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let hash = *transaction.hash();
        self.broadcast_hash_event(&hash, TransactionEvent::Pending);
        self.broadcast_all(FullTransactionEvent::Pending(hash));
        self.broadcast_pending(transaction);
        self.broadcast_new(NewTransactionEvent::pending(Arc::clone(transaction)));
    }

    fn broadcast_pending(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let hash = *transaction.hash();
        Self::send_to_open(&mut self.pending_all, &hash);
        if transaction.propagate {
            Self::send_to_open(&mut self.pending_propagate, &hash);
        }
    }

    fn broadcast_new(&mut self, event: NewTransactionEvent<T>) {
        Self::send_to_open(&mut self.new_all, &event);
        if event.transaction.propagate {
            Self::send_to_open(&mut self.new_propagate, &event);
        }
    }

    /// Sends `item` to every listener without waiting, dropping only listeners whose receiver is
    /// gone.
    ///
    /// A full channel drops the item but keeps the listener: a consumer that falls behind for a
    /// while must not lose its subscription for good, since a merged listener stays open on the
    /// protocol side and the consumer would never learn that sidecar events stopped.
    fn send_to_open<I: Clone>(listeners: &mut Vec<mpsc::Sender<I>>, item: &I) {
        listeners.retain(|listener| {
            !matches!(listener.try_send(item.clone()), Err(TrySendError::Closed(_)))
        });
    }
}

fn merge_receivers<T: Send + 'static>(
    mut left: mpsc::Receiver<T>,
    mut right: mpsc::Receiver<T>,
) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
    spawn(async move {
        let mut left_open = true;
        let mut right_open = true;
        while left_open || right_open {
            tokio::select! {
                item = left.recv(), if left_open => match item {
                    Some(item) => {
                        if tx.send(item).await.is_err() {
                            break;
                        }
                    }
                    None => left_open = false,
                },
                item = right.recv(), if right_open => match item {
                    Some(item) => {
                        if tx.send(item).await.is_err() {
                            break;
                        }
                    }
                    None => right_open = false,
                }
            }
        }
    });
    rx
}

fn pooled_element<T: BasePooledTx>(
    transaction: &Arc<ValidPoolTransaction<T>>,
) -> Option<(<T as PoolTransaction>::Pooled, usize)> {
    transaction
        .transaction
        .clone()
        .try_into_pooled()
        .ok()
        .map(|recovered| recovered.into_parts().0)
        .map(|pooled| (pooled, transaction.encoded_length()))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use alloy_consensus::{
        SignableTransaction, Transaction, TxEip1559,
        transaction::{Recovered, SignerRecoverable},
    };
    use alloy_eips::{eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M, eip2718::Encodable2718};
    use alloy_primitives::{Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BaseBlock, BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives,
        BaseTransactionSigned, BaseTxEnvelope, Call, Eip8130Constants, Eip8130Signed, Predeploys,
        TxDeposit, TxEip8130,
    };
    use base_common_evm::BaseSpecId;
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use base_test_utils::build_test_genesis_everest;
    use futures::{FutureExt, StreamExt, future::join_all};
    use reth_primitives_traits::{InMemorySize, SealedBlock};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_tasks::Runtime;
    use reth_transaction_pool::{
        CanonicalStateUpdate, PoolConfig, PoolUpdateKind, PriceBumpConfig, SubPoolLimit,
        TransactionOrigin, blobstore::InMemoryBlobStore, error::PoolErrorKind,
        identifier::TransactionId, validate::EthTransactionValidatorBuilder,
    };

    use super::*;
    use crate::{
        BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BuilderApiImpl, BuilderApiServer,
        LimitClass, NoExtensions, ValidatedTransaction, ValidityOperator, ValidityPredicate,
        WatchSet,
    };

    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    #[test]
    fn account_state_diff_derives_only_exact_invalidation_keys() {
        let address = Address::repeat_byte(7);
        let slot = B256::repeat_byte(8);
        let diff = AccountStateDiff {
            address,
            balance: Some(U256::from(1)),
            nonce: Some(3),
            code_changed: true,
            changed_slots: vec![(slot, U256::from(9))],
        };
        let (mut keys, mut advances) = (Vec::new(), Vec::new());
        diff.push_exact_keys(&mut keys);
        diff.push_nonce_advances(&mut advances);
        assert_eq!(
            keys,
            vec![InvalidationKey::CodeHash(address), InvalidationKey::Slot { address, slot }]
        );
        assert_eq!(advances, vec![(InvalidationKey::ProtocolNonce(address), 3)]);

        let nonce_manager = AccountStateDiff {
            address: NonceManagerStorage::ADDRESS,
            changed_slots: vec![(slot, U256::from(9))],
            ..Default::default()
        };
        let (mut keys, mut advances) = (Vec::new(), Vec::new());
        nonce_manager.push_exact_keys(&mut keys);
        nonce_manager.push_nonce_advances(&mut advances);
        assert!(keys.is_empty());
        assert_eq!(
            advances,
            vec![(InvalidationKey::Slot { address: NonceManagerStorage::ADDRESS, slot }, 9)]
        );
    }

    #[test]
    fn nonce_free_sidecar_builds_guard_admission() {
        let signer = signer();
        let transaction = signed_nonce_free_tx(&signer, 10, 1_000);
        let address = signer.address();
        transaction.set_watch_set(WatchSet::new().watch(InvalidationKey::ProtocolNonce(address)));
        transaction.set_limit_class(LimitClass {
            sender: address,
            payer: address,
            classification_generation: 0,
            sender_locked: false,
            payer_locked: false,
            payer_trusted: false,
            payer_allowlisted: false,
            payer_balance: U256::from(1_000_000),
            max_cost: U256::from(1_000),
        });

        let admission = IntegrationPool::admission_for(&transaction).expect("EIP-8130 admission");
        assert_eq!(admission.hash, *transaction.hash());
        assert_eq!(admission.sender, address);
    }

    fn signed_channel_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_8130(signer, nonce_key, nonce_sequence, 0, max_fee_per_gas, 50_000)
    }

    fn signed_8130(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        valid_before: u64,
        max_fee_per_gas: u128,
        gas_limit: u64,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas,
            gas_limit,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        sign_eoa_8130(signer, tx)
    }

    fn sign_eoa_8130(signer: &PrivateKeySigner, tx: TxEip8130) -> BasePooledTransaction {
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn sponsored_eoa_8130(
        signer: &PrivateKeySigner,
        payer: &PrivateKeySigner,
        nonce_key: U256,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas,
            gas_limit: 1_000_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: Some(payer.address()),
        };
        sign_sponsored_8130(signer, payer, tx)
    }

    fn sign_sponsored_8130(
        signer: &PrivateKeySigner,
        payer: &PrivateKeySigner,
        tx: TxEip8130,
    ) -> BasePooledTransaction {
        let sender_auth = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let payer_sig = payer.sign_hash_sync(&tx.payer_signature_hash(signer.address())).unwrap();
        let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
        payer_auth.extend_from_slice(&payer_sig.as_bytes());
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sender_auth.as_bytes().to_vec()),
            Bytes::from(payer_auth),
        );
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn signed_nonce_free_tx(
        signer: &PrivateKeySigner,
        expiry: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_8130(signer, Eip8130Constants::NONCE_KEY_MAX, 0, expiry, max_fee_per_gas, 50_000)
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        }
    }

    #[tokio::test]
    async fn hash_subscriber_receives_initial_pending_event_for_new_sidecar_transaction() {
        let mut nonce_pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let mut listeners = SidecarListeners::default();
        let signer = signer();

        let transaction =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let hash = *transaction.hash();

        let mut events = listeners.subscribe_hash(hash).0;
        let outcome = nonce_pool.insert_validated(transaction, 0).unwrap();
        listeners.on_inserted(&nonce_pool, &outcome);

        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
    }

    #[tokio::test]
    async fn gap_fill_broadcasts_pending_for_promoted_sidecar_transaction() {
        let mut nonce_pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let mut listeners = SidecarListeners::default();
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 2, 800));
        let queued_hash = *queued.hash();
        let middle = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 1, 900));
        let middle_hash = *middle.hash();

        nonce_pool.insert_validated(first, 0).unwrap();
        nonce_pool.insert_validated(queued, 0).unwrap();

        let mut pending = listeners.subscribe_pending(TransactionListenerKind::All);
        let mut queued_events = listeners.subscribe_hash(queued_hash).0;

        let outcome = nonce_pool.insert_validated(middle, 0).unwrap();
        listeners.on_inserted(&nonce_pool, &outcome);

        assert_eq!(pending.recv().await, Some(middle_hash));
        assert_eq!(pending.recv().await, Some(queued_hash));
        assert!(matches!(queued_events.next().await, Some(TransactionEvent::Pending)));
    }

    #[tokio::test]
    async fn discarded_sidecar_transaction_broadcasts_terminal_events() {
        let mut listeners = SidecarListeners::default();
        let signer = signer();
        let transaction =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(2), 0, 1_000)));
        let hash = *transaction.hash();

        let mut hash_events = listeners.subscribe_hash(hash).0;
        let mut all_events = listeners.subscribe_all();

        listeners.on_discarded(&[transaction]);

        assert!(matches!(hash_events.next().await, Some(TransactionEvent::Discarded)));
        assert!(
            matches!(all_events.next().await, Some(FullTransactionEvent::Discarded(event_hash)) if event_hash == hash)
        );
    }

    #[tokio::test]
    async fn nonce_free_replacement_notifies_old_hash_and_keeps_new_pending() {
        let mut nonce_pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let mut listeners = SidecarListeners::default();
        let signer = signer();
        let original = valid_pool_transaction(signed_nonce_free_tx(&signer, 10, 1_000));
        let replacement = valid_pool_transaction(signed_nonce_free_tx(&signer, 10, 1_250));
        let original_hash = *original.hash();
        let replacement_hash = *replacement.hash();
        let mut original_events = listeners.subscribe_hash(original_hash).0;
        let mut replacement_events = listeners.subscribe_hash(replacement_hash).0;

        let original_outcome = nonce_pool.insert_validated(original, 0).unwrap();
        listeners.on_inserted(&nonce_pool, &original_outcome);
        assert!(matches!(original_events.next().await, Some(TransactionEvent::Pending)));

        let replacement_outcome = nonce_pool.insert_validated(replacement, 0).unwrap();
        listeners.on_inserted(&nonce_pool, &replacement_outcome);

        assert!(
            matches!(original_events.next().await, Some(TransactionEvent::Replaced(hash)) if hash == replacement_hash)
        );
        assert!(matches!(replacement_events.next().await, Some(TransactionEvent::Pending)));
        assert!(nonce_pool.get(&original_hash).is_none());
        assert!(nonce_pool.get(&replacement_hash).is_some());
    }

    type IntegrationPool = BaseTransactionPool<
        MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        InMemoryBlobStore,
        BaseEvmConfig,
    >;

    /// Realistic wall clock the integration pool starts at (Unix seconds), so
    /// millisecond `valid_before` bounds are compared against a representative
    /// `block.timestamp * 1000` rather than the epoch.
    const INTEGRATION_POOL_NOW_SECS: u64 = 1_700_000_000;
    /// [`INTEGRATION_POOL_NOW_SECS`] expressed in milliseconds.
    const INTEGRATION_POOL_NOW_MS: u64 = INTEGRATION_POOL_NOW_SECS * 1_000;

    fn build_integration_pool()
    -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_at(false, PoolConfig::default(), false)
    }

    /// [`build_integration_pool`] with the Keystore active (Zenith at genesis).
    fn build_zenith_integration_pool()
    -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_at(true, PoolConfig::default(), false)
    }

    fn build_integration_pool_with_config(
        config: PoolConfig,
    ) -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_at(false, config, false)
    }

    /// [`build_integration_pool`] charging the L1 data and operator fees of
    /// [`ISTHMUS_L1_INFO_CALLDATA`].
    fn build_integration_pool_with_l1_fees()
    -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_at(false, PoolConfig::default(), true)
    }

    /// Operator pool policies the node passes to the validator.
    struct PoolPolicies {
        block_gas_limit: u64,
        max_tx_gas_limit: Option<u64>,
        minimum_priority_fee: Option<u128>,
        tx_fee_cap: u128,
    }

    impl Default for PoolPolicies {
        /// The validator builder's defaults.
        fn default() -> Self {
            Self {
                block_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
                max_tx_gas_limit: None,
                minimum_priority_fee: None,
                tx_fee_cap: 1_000_000_000_000_000_000,
            }
        }
    }

    /// [`build_integration_pool`] with the given operator pool policies.
    fn build_integration_pool_with_policies(
        policies: PoolPolicies,
    ) -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_full(false, PoolConfig::default(), false, policies)
    }

    fn build_integration_pool_at(
        zenith: bool,
        config: PoolConfig,
        l1_fees: bool,
    ) -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_full(zenith, config, l1_fees, PoolPolicies::default())
    }

    fn build_integration_pool_full(
        zenith: bool,
        config: PoolConfig,
        l1_fees: bool,
        policies: PoolPolicies,
    ) -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        let mut genesis = build_test_genesis_everest();
        genesis.config.chain_id = test_chain_id();
        if zenith {
            genesis.config.extra_fields.insert(
                "base".to_string(),
                serde_json::json!({
                    "azul": 0, "beryl": 0, "cobalt": 0, "denim": 0, "everest": 0, "zenith": 0
                }),
            );
        }
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
            .no_shanghai()
            .no_cancun()
            .set_block_gas_limit(policies.block_gas_limit)
            .with_max_tx_gas_limit(policies.max_tx_gas_limit)
            .with_minimum_priority_fee(policies.minimum_priority_fee)
            .set_tx_fee_cap(policies.tx_fee_cap)
            .build_with_tasks(Runtime::test(), blob_store.clone())
            .map(|inner| {
                // Seed a realistic wall clock so millisecond-scale nonce-free
                // `valid_before` bounds evaluate against a representative `now`
                // (default is timestamp 0, where no future ms bound is reachable).
                // Reuse the same seam the live head uses: `update_l1_block_info`
                // with no transactions stores only the header timestamp and leaves the
                // L1 block info untouched, so the harness shares the one admission
                // clock writer rather than a second, unsynchronized setter.
                let validator =
                    BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
                        .require_l1_data_gas_fee(l1_fees)
                        .with_max_tx_gas_limit(policies.max_tx_gas_limit);
                let header = alloy_consensus::Header {
                    timestamp: INTEGRATION_POOL_NOW_SECS,
                    ..Default::default()
                };
                if l1_fees {
                    validator.update_l1_block_info(&header, &[l1_info_deposit()]);
                } else {
                    validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
                }
                validator
            });
        let ordering = BaseOrdering::default();
        let pool = Pool::new(validator, ordering.clone(), blob_store, config);
        (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
    }

    fn fund(client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>, account: Address) {
        client.add_account(
            account,
            ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)),
        );
    }

    fn publish(pool: &IntegrationPool, block: u64, flashblock: u64) -> usize {
        pool.publish_and_expire(block, flashblock, || Ok::<(), ()>(())).unwrap().1
    }

    fn signed_1559(signer: &PrivateKeySigner, nonce: u64) -> BasePooledTransaction {
        signed_1559_with_fees(signer, nonce, 1_000, 0)
    }

    fn signed_1559_with_fees(
        signer: &PrivateKeySigner,
        nonce: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip1559 {
            chain_id: test_chain_id(),
            nonce,
            gas_limit: 50_000,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(Address::repeat_byte(0xEE)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered = envelope.clone().try_into_recovered().unwrap();
        BasePooledTransaction::new(recovered, envelope.encode_2718_len())
    }

    fn self_paid_eoa_8130(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        expiry: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_8130(signer, nonce_key, nonce_sequence, expiry, max_fee_per_gas, 1_000_000)
    }

    #[tokio::test]
    async fn sender_snapshot_includes_both_nonce_lanes_and_filters_other_senders() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        let other = signer();
        fund(&client, sender.address());
        fund(&client, other.address());

        let protocol_pending = signed_1559(&sender, 0);
        let protocol_queued = signed_1559(&sender, 2);
        let sidecar_pending = signed_channel_tx(&sender, U256::from(1), 0, 1_000);
        let sidecar_queued = signed_channel_tx(&sender, U256::from(1), 2, 1_000);
        let mut expected_pending = vec![*protocol_pending.hash(), *sidecar_pending.hash()];
        let mut expected_queued = vec![*protocol_queued.hash(), *sidecar_queued.hash()];

        for transaction in [
            protocol_pending,
            protocol_queued,
            sidecar_pending,
            sidecar_queued,
            signed_1559(&other, 0),
        ] {
            pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        }

        let snapshot = pool.all_transactions_by_sender(sender.address());
        let mut pending =
            snapshot.pending.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>();
        let mut queued =
            snapshot.queued.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>();
        pending.sort_unstable();
        queued.sort_unstable();
        expected_pending.sort_unstable();
        expected_queued.sort_unstable();
        assert_eq!(pending, expected_pending);
        assert_eq!(queued, expected_queued);
    }

    #[tokio::test]
    async fn batch_add_matches_sequential_add() {
        let (funded, unfunded) = (signer(), signer());
        let batch = [
            signed_1559(&funded, 0),
            signed_1559(&funded, 1),
            signed_1559(&funded, 0),
            signed_1559(&funded, 3),
            signed_1559(&unfunded, 0),
            signed_1559(&funded, 2),
        ];
        let batch = batch.map(|tx| (TransactionOrigin::Local, tx)).to_vec();
        let (batched, pool) = assert_batch_matches_sequential(batch, &[funded.address()]).await;
        assert!(
            matches!(batched[4], Err(ref e) if matches!(e.kind, PoolErrorKind::InvalidTransaction(_)))
        );
        assert_eq!(pool.pool_size().pending, 4);
    }

    #[tokio::test]
    async fn batch_add_matches_sequential_add_for_in_batch_replacements() {
        let sender = signer();
        let replacement = signed_1559_with_fees(&sender, 0, 2_000, 0);
        let replacement_hash = *replacement.hash();
        // A 5% bump over the replacement is below the pool's 10% price bump.
        let batch = [
            signed_1559_with_fees(&sender, 0, 1_000, 0),
            replacement,
            signed_1559_with_fees(&sender, 0, 2_100, 0),
        ];
        let batch = batch.map(|tx| (TransactionOrigin::Local, tx)).to_vec();
        let (batched, pool) = assert_batch_matches_sequential(batch, &[sender.address()]).await;
        assert!(batched[1].is_ok());
        assert!(
            matches!(batched[2], Err(ref e) if matches!(e.kind, PoolErrorKind::ReplacementUnderpriced))
        );
        assert!(pool.get(&replacement_hash).is_some());
    }

    #[tokio::test]
    async fn batch_add_matches_sequential_add_with_mixed_origins() {
        let (first, second) = (signer(), signer());
        let batch = vec![
            (TransactionOrigin::Local, signed_1559(&first, 0)),
            (TransactionOrigin::External, signed_1559(&second, 0)),
            (TransactionOrigin::Private, signed_1559(&first, 1)),
        ];
        let expected: Vec<_> = batch.iter().map(|(origin, tx)| (*tx.hash(), *origin)).collect();
        let (batched, pool) =
            assert_batch_matches_sequential(batch, &[first.address(), second.address()]).await;
        assert!(batched.iter().all(Result::is_ok));
        for (hash, origin) in expected {
            assert_eq!(pool.get(&hash).expect("pooled").origin, origin);
        }
    }

    #[tokio::test]
    async fn batch_with_a_sidecar_transaction_matches_sequential_add() {
        let sender = signer();
        let sidecar = self_paid_eoa_8130(&sender, U256::from(2), 0, 0, 1_000);
        let sidecar_hash = *sidecar.hash();
        let batch = vec![
            (TransactionOrigin::Local, signed_1559(&sender, 0)),
            (TransactionOrigin::Local, sidecar),
            (TransactionOrigin::Local, signed_1559(&sender, 1)),
        ];
        let (batched, pool) = assert_batch_matches_sequential(batch, &[sender.address()]).await;
        assert!(batched.iter().all(Result::is_ok));
        assert!(pool.sidecar.pool.read().contains(&sidecar_hash));
    }

    /// Adds `batch` one transaction at a time to one fresh pool and as one
    /// batch to another, both funding `funded`, and asserts the same outcome
    /// per transaction and the same pool size. Returns the batched outcomes
    /// and pool.
    async fn assert_batch_matches_sequential(
        batch: Vec<(TransactionOrigin, BasePooledTransaction)>,
        funded: &[Address],
    ) -> (Vec<PoolResult<AddedTransactionOutcome>>, IntegrationPool) {
        // `PoolErrorKind` is not `PartialEq`; compare errors by hash and message.
        let comparable = |results: &[PoolResult<AddedTransactionOutcome>]| {
            results
                .iter()
                .map(|result| match result {
                    Ok(outcome) => Ok(outcome.clone()),
                    Err(error) => Err((error.hash, error.kind.to_string())),
                })
                .collect::<Vec<_>>()
        };
        let (sequential_pool, client) = build_integration_pool();
        funded.iter().for_each(|account| fund(&client, *account));
        let mut sequential = Vec::new();
        for (origin, transaction) in batch.clone() {
            sequential.push(sequential_pool.add_transaction(origin, transaction).await);
        }

        let (batch_pool, client) = build_integration_pool();
        funded.iter().for_each(|account| fund(&client, *account));
        let batched = batch_pool.add_transactions_with_origins(batch).await;

        assert_eq!(comparable(&batched), comparable(&sequential));
        let size = |pool: &IntegrationPool| (pool.pool_size().pending, pool.pool_size().queued);
        assert_eq!(size(&batch_pool), size(&sequential_pool));
        (batched, batch_pool)
    }

    #[tokio::test]
    async fn rpc_batch_insert_matches_single_inserts() {
        let (funded, unfunded) = (signer(), signer());
        let wire = |tx: BasePooledTransaction| {
            let recovered = tx.clone_into_consensus();
            ValidatedTransaction {
                sender: recovered.signer(),
                raw: recovered.inner().encoded_2718().into(),
                metering: None,
                extensions: NoExtensions::default(),
            }
        };
        let mut undecodable = wire(signed_1559(&funded, 4));
        undecodable.raw = Bytes::from_static(&[0x02, 0xff]);
        let batch = vec![
            wire(signed_1559(&funded, 0)),
            wire(signed_1559(&funded, 1)),
            wire(signed_1559(&funded, 0)),
            undecodable,
            wire(signed_1559(&unfunded, 0)),
            wire(signed_1559(&funded, 2)),
        ];

        let (single_pool, client) = build_integration_pool();
        fund(&client, funded.address());
        let single = BuilderApiImpl::new(single_pool.clone());
        let mut expected = Vec::new();
        for tx in batch.clone() {
            expected.push(single.insert_validated_transaction(tx).await.err());
        }

        let (batch_pool, client) = build_integration_pool();
        fund(&client, funded.address());
        let batched =
            BuilderApiImpl::new(batch_pool.clone()).insert_validated_transactions(batch).await;

        assert_eq!(batched.unwrap(), expected);
        assert_eq!(
            expected.iter().map(Option::is_none).collect::<Vec<_>>(),
            [true, true, false, false, false, true]
        );
        let size = |pool: &IntegrationPool| (pool.pool_size().pending, pool.pool_size().queued);
        assert_eq!(size(&batch_pool), size(&single_pool));
        assert_eq!(size(&batch_pool), (3, 0));
    }

    #[tokio::test]
    async fn rpc_insert_reports_pooled_duplicates_as_already_imported() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        fund(&client, sender.address());
        let tx = signed_1559(&sender, 0);
        let recovered = tx.clone_into_consensus();
        let wire = ValidatedTransaction {
            sender: recovered.signer(),
            raw: recovered.inner().encoded_2718().into(),
            metering: None,
            extensions: NoExtensions::default(),
        };
        let handler = BuilderApiImpl::new(pool.clone());
        assert_eq!(handler.insert_validated_transaction(wire.clone()).await, Ok(()));

        // Validating the duplicate now would fail for lack of funds.
        client.add_account(sender.address(), ExtendedAccount::new(0, U256::ZERO));
        let expected = format!(
            "pool rejected transaction: {}",
            reth_transaction_pool::error::PoolError::new(
                *tx.hash(),
                PoolErrorKind::AlreadyImported
            )
        );
        let single = handler.insert_validated_transaction(wire.clone()).await.unwrap_err();
        let batched = handler.insert_validated_transactions(vec![wire]).await.unwrap();

        assert_eq!(single.message(), expected);
        assert_eq!(batched, vec![Some(single)]);
        assert_eq!(pool.pool_size().pending, 1);
    }

    #[tokio::test]
    async fn standard_transactions_are_not_eip8130_gated() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let count = u64::from(GuardLimits::default().signature_limit) + 2;
        for nonce in 0..count {
            let result =
                pool.add_transaction(TransactionOrigin::Local, signed_1559(&signer, nonce)).await;
            assert!(result.is_ok(), "standard transaction {nonce} was guard-rejected: {result:?}");
        }
        assert!(pool.guard.read().is_empty());
    }

    #[tokio::test]
    async fn protocol_eip8130_validity_replacement_requires_the_price_bump() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let predicate =
            ValidityPredicate::BlockNumber { op: ValidityOperator::LessThan, value: U256::from(2) };
        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000)
            .with_validity_predicates(vec![predicate.clone()]);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        let underpriced = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_001)
            .with_validity_predicates(vec![predicate.clone()]);
        let err = pool
            .add_transaction(TransactionOrigin::Local, underpriced)
            .await
            .expect_err("an EIP-8130 validity replacement still needs the price bump");
        assert!(matches!(err.kind, PoolErrorKind::ReplacementUnderpriced), "got {err:?}");

        let replacement = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_100)
            .with_validity_predicates(vec![predicate]);
        let replacement_hash = *replacement.hash();
        pool.add_transaction(TransactionOrigin::Local, replacement)
            .await
            .expect("a bumped EIP-8130 validity replacement is accepted");

        assert!(pool.get(&original_hash).is_none());
        assert!(pool.get(&replacement_hash).is_some());
    }

    #[tokio::test]
    async fn protocol_and_sidecar_eip8130_routes_enforce_sender_limit() {
        let cap = u64::from(GuardLimits::default().signature_limit);

        let (protocol_pool, protocol_client) = build_zenith_integration_pool();
        let protocol_signer = signer();
        fund(&protocol_client, protocol_signer.address());
        for sequence in 0..cap {
            let transaction = self_paid_eoa_8130(&protocol_signer, U256::ZERO, sequence, 0, 1_000);
            assert!(
                protocol_pool.add_transaction(TransactionOrigin::Local, transaction).await.is_ok()
            );
        }
        let mut protocol_events = protocol_pool.all_transactions_event_listener();
        let over = self_paid_eoa_8130(&protocol_signer, U256::ZERO, cap, 0, 1_000);
        let over_hash = *over.hash();
        let error = protocol_pool
            .add_transaction(TransactionOrigin::Local, over)
            .await
            .expect_err("protocol-resident transaction above the cap must be rejected");
        assert!(error.to_string().contains("sender EIP-8130 signature limit"));
        assert!(protocol_pool.get(&over_hash).is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), protocol_events.next()).await.is_err(),
            "guard-rejected protocol transaction must not publish pool events"
        );

        let (sidecar_pool, sidecar_client) = build_zenith_integration_pool();
        let sidecar_signer = signer();
        fund(&sidecar_client, sidecar_signer.address());
        for key in 1..=cap {
            let transaction = self_paid_eoa_8130(&sidecar_signer, U256::from(key), 0, 0, 1_000);
            assert!(
                sidecar_pool.add_transaction(TransactionOrigin::Local, transaction).await.is_ok()
            );
        }
        let over = self_paid_eoa_8130(&sidecar_signer, U256::from(cap + 1), 0, 0, 1_000);
        let over_hash = *over.hash();
        let mut over_events = sidecar_pool.sidecar.listeners.write().subscribe_hash(over_hash).0;
        let error = sidecar_pool
            .add_transaction(TransactionOrigin::Local, over)
            .await
            .expect_err("sidecar transaction above the cap must be rejected");
        assert!(error.to_string().contains("sender EIP-8130 signature limit"));
        assert!(sidecar_pool.get(&over_hash).is_none());
        assert!(matches!(over_events.next().await, Some(TransactionEvent::Discarded)));
    }

    /// Without the Keystore a signer's key can never change, so the sender
    /// signature limit does not apply: a sender may queue past it.
    #[tokio::test]
    async fn sender_signature_limit_does_not_apply_before_zenith() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let cap = u64::from(GuardLimits::default().signature_limit);
        for sequence in 0..cap + 2 {
            let transaction = self_paid_eoa_8130(&signer, U256::ZERO, sequence, 0, 1_000);
            assert!(pool.add_transaction(TransactionOrigin::Local, transaction).await.is_ok());
        }
    }

    #[tokio::test]
    async fn nonce_free_sidecar_members_are_guarded_independently() {
        let (pool, client) = build_zenith_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let cap = u64::from(GuardLimits::default().signature_limit);

        // Distinct millisecond `valid_before` bounds, each inside the nonce-free
        // admission window `(now_ms, now_ms + NONCE_FREE_MAX_EXPIRY_WINDOW]`, so
        // each is a separately-tracked sidecar member; `cap` stays far below the
        // 20_000 ms window.
        let mut admitted = Vec::new();
        for offset in 0..cap {
            let transaction = self_paid_eoa_8130(
                &signer,
                Eip8130Constants::NONCE_KEY_MAX,
                0,
                INTEGRATION_POOL_NOW_MS + offset + 1,
                1_000,
            );
            admitted.push(*transaction.hash());
            assert!(pool.add_transaction(TransactionOrigin::Local, transaction).await.is_ok());
        }
        assert!(admitted.iter().all(|hash| pool.sidecar.pool.read().contains(hash)));

        let over = self_paid_eoa_8130(
            &signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            INTEGRATION_POOL_NOW_MS + cap + 1,
            1_000,
        );
        let over_hash = *over.hash();
        assert!(pool.add_transaction(TransactionOrigin::Local, over).await.is_err());
        assert!(pool.get(&over_hash).is_none());

        let removed = pool.remove_transactions(vec![admitted[0]]);
        assert_eq!(removed.len(), 1);
        let replacement = self_paid_eoa_8130(
            &signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            INTEGRATION_POOL_NOW_MS + cap + 2,
            1_000,
        );
        assert!(pool.add_transaction(TransactionOrigin::Local, replacement).await.is_ok());
    }

    #[tokio::test]
    async fn protocol_replacement_at_limit_is_forced_and_reaccounted() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let cap = u64::from(GuardLimits::default().signature_limit);

        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();
        for sequence in 1..cap {
            pool.add_transaction(
                TransactionOrigin::Local,
                self_paid_eoa_8130(&signer, U256::ZERO, sequence, 0, 1_000),
            )
            .await
            .unwrap();
        }

        let replacement = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_250);
        let replacement_hash = *replacement.hash();
        pool.add_transaction(TransactionOrigin::Local, replacement)
            .await
            .expect("fee bump must remain possible at the admission cap");

        assert!(pool.get(&original_hash).is_none());
        assert!(pool.get(&replacement_hash).is_some());
        assert!(!pool.guard.read().contains(&original_hash));
        assert!(pool.guard.read().contains(&replacement_hash));
        assert_eq!(pool.guard.read().len(), cap as usize);
    }

    /// A fee bump that names a new payer is held to that payer's payment cap,
    /// on both the protocol (key 0) and sidecar (2D channel) routes. A rejected
    /// rotation keeps the original transaction pooled and tracked.
    #[tokio::test]
    async fn replacement_cannot_rotate_to_a_payer_at_its_payment_cap() {
        for nonce_key in [U256::ZERO, U256::from(1)] {
            let (pool, client) = build_integration_pool();
            let pool =
                pool.with_guard_limits(GuardLimits { payment_limit: 1, ..GuardLimits::default() });
            let [sender, other, full, original_payer, open] = std::array::from_fn(|_| signer());
            for account in [&sender, &other, &full, &original_payer, &open] {
                fund(&client, account.address());
            }
            pool.add_transaction(
                TransactionOrigin::Local,
                sponsored_eoa_8130(&other, &full, nonce_key, 1_000),
            )
            .await
            .unwrap();
            let original = sponsored_eoa_8130(&sender, &original_payer, nonce_key, 1_000);
            let original_hash = *original.hash();
            pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

            let rotated = sponsored_eoa_8130(&sender, &full, nonce_key, 1_250);
            let rotated_hash = *rotated.hash();
            let error = pool
                .add_transaction(TransactionOrigin::Local, rotated)
                .await
                .expect_err("rotating to a payer at its cap must be rejected");
            assert!(error.to_string().contains("payment limit"), "{nonce_key}: {error}");
            assert!(pool.get(&rotated_hash).is_none());
            assert!(pool.get(&original_hash).is_some());
            assert!(pool.guard.read().contains(&original_hash));

            let bumped = sponsored_eoa_8130(&sender, &open, nonce_key, 1_250);
            let bumped_hash = *bumped.hash();
            pool.add_transaction(TransactionOrigin::Local, bumped)
                .await
                .expect("rotating to a payer with room is allowed");
            assert!(pool.get(&original_hash).is_none());
            assert!(pool.guard.read().contains(&bumped_hash));
        }
    }

    #[tokio::test]
    async fn concurrent_protocol_replacements_leave_no_stale_guard_records() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let transactions = (0..16)
            .map(|index| self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000 + index * 250))
            .collect::<Vec<_>>();

        let outcomes = join_all(
            transactions
                .into_iter()
                .map(|transaction| pool.add_transaction(TransactionOrigin::Local, transaction)),
        )
        .await;

        assert!(outcomes.iter().any(Result::is_ok));
        let tracked = pool.guard.read().tracked_hashes();
        assert_eq!(tracked.len(), 1, "same-nonce replacements must consume one guard slot");
        assert!(
            tracked.iter().all(|hash| pool.protocol_pool.get(hash).is_some()),
            "every guard record must still have a protocol-pool transaction"
        );
    }

    #[tokio::test]
    async fn stale_protocol_replacement_keeps_original_transaction() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        let replacement = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_250);
        let replacement_hash = *replacement.hash();
        let replacement_sender = replacement.sender();
        let replacement_nonce = replacement.nonce();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::Local, replacement).await;
        pool.validator().validator().clear_limit_class_cache();

        let error = pool
            .add_validated_protocol_transaction(
                TransactionOrigin::Local,
                replacement_hash,
                replacement_sender,
                replacement_nonce,
                validated,
            )
            .expect_err("stale classification must reject before replacing the pooled transaction");

        assert!(error.to_string().contains("classification changed during validation"));
        assert!(pool.get(&replacement_hash).is_none());
        assert!(pool.get(&original_hash).is_some());
        assert!(pool.guard.read().contains(&original_hash));
        assert!(!pool.guard.read().contains(&replacement_hash));
    }

    #[tokio::test]
    async fn full_sidecar_evicts_the_cheapest_channel_transaction() {
        let config =
            PoolConfig { pending_limit: SubPoolLimit::new(1, usize::MAX), ..PoolConfig::default() };
        let (pool, client) = build_integration_pool_with_config(config);
        let incumbent_signer = signer();
        let cheap_signer = signer();
        let rich_signer = signer();
        for signer in [&incumbent_signer, &cheap_signer, &rich_signer] {
            fund(&client, signer.address());
        }

        let incumbent = self_paid_eoa_8130(&incumbent_signer, U256::from(1), 0, 0, 2_000);
        let incumbent_hash = *incumbent.hash();
        pool.add_transaction(TransactionOrigin::External, incumbent).await.unwrap();
        let mut incumbent_events = pool.sidecar.listeners.write().subscribe_hash(incumbent_hash).0;

        let cheap = self_paid_eoa_8130(&cheap_signer, U256::from(1), 0, 0, 1_000);
        let cheap_hash = *cheap.hash();
        let error = pool
            .add_transaction(TransactionOrigin::External, cheap)
            .await
            .expect_err("a full sidecar must not admit a cheaper transaction");
        assert!(matches!(error.kind, PoolErrorKind::DiscardedOnInsert));
        assert!(pool.get(&cheap_hash).is_none());
        assert!(!pool.guard.read().contains(&cheap_hash));
        assert!(pool.get(&incumbent_hash).is_some());

        let rich = self_paid_eoa_8130(&rich_signer, U256::from(1), 0, 0, 3_000);
        let rich_hash = *rich.hash();
        pool.add_transaction(TransactionOrigin::External, rich).await.unwrap();
        assert!(pool.get(&rich_hash).is_some());
        assert!(pool.get(&incumbent_hash).is_none());
        assert!(!pool.guard.read().contains(&incumbent_hash));
        assert!(matches!(incumbent_events.next().await, Some(TransactionEvent::Discarded)));
    }

    #[tokio::test]
    async fn sidecar_pool_size_reports_in_memory_size() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let transaction = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        let pooled = pool.get(&hash).expect("sidecar holds the transaction");

        let reported = pool.pool_size();
        assert_eq!(reported.pending, 1);
        assert_eq!(reported.pending_size, pooled.transaction.size());
        assert_ne!(reported.pending_size, pooled.encoded_length());
    }

    #[tokio::test]
    async fn promoted_transaction_evicted_on_insert_is_not_announced_pending() {
        let config =
            PoolConfig { pending_limit: SubPoolLimit::new(2, usize::MAX), ..PoolConfig::default() };
        let (pool, client) = build_integration_pool_with_config(config);
        let lane_signer = signer();
        let other_signer = signer();
        fund(&client, lane_signer.address());
        fund(&client, other_signer.address());

        let cheap_queued = self_paid_eoa_8130(&lane_signer, U256::from(1), 1, 0, 1_000);
        let cheap_queued_hash = *cheap_queued.hash();
        pool.add_transaction(TransactionOrigin::External, cheap_queued).await.unwrap();
        let other = self_paid_eoa_8130(&other_signer, U256::from(1), 0, 0, 5_000);
        pool.add_transaction(TransactionOrigin::External, other).await.unwrap();
        let mut pending =
            pool.sidecar.listeners.write().subscribe_pending(TransactionListenerKind::All);

        let gap_filler = self_paid_eoa_8130(&lane_signer, U256::from(1), 0, 0, 5_000);
        let gap_filler_hash = *gap_filler.hash();
        pool.add_transaction(TransactionOrigin::External, gap_filler).await.unwrap();

        assert!(pool.get(&cheap_queued_hash).is_none());
        let mut announced = Vec::new();
        while let Ok(hash) = pending.try_recv() {
            announced.push(hash);
        }
        assert_eq!(announced, vec![gap_filler_hash]);
    }

    #[tokio::test]
    async fn sidecar_rejects_fee_cap_below_the_protocol_floor() {
        let config = PoolConfig { minimal_protocol_basefee: 1_001, ..PoolConfig::default() };
        let (pool, client) = build_integration_pool_with_config(config);
        let signer = signer();
        fund(&client, signer.address());

        let transaction = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        let error = pool
            .add_transaction(TransactionOrigin::External, transaction)
            .await
            .expect_err("the sidecar must apply the protocol fee floor");
        assert!(matches!(error.kind, PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(1_000)));
    }

    /// Gas limit of the transactions in the operator pool policy tests.
    const POLICY_GAS_LIMIT: u64 = 100_000;
    /// `max_fee_per_gas` of the transactions in the operator pool policy tests.
    const POLICY_MAX_FEE: u128 = 1_000;
    /// The protocol lane (nonce key zero) and a 2D nonce sidecar channel.
    const EIP8130_ROUTES: [U256; 2] = [U256::ZERO, U256::from_limbs([1, 0, 0, 0])];

    fn policy_tx(nonce_key: U256) -> TxEip8130 {
        TxEip8130 {
            chain_id: test_chain_id(),
            nonce_key,
            max_fee_per_gas: POLICY_MAX_FEE,
            gas_limit: POLICY_GAS_LIMIT,
            ..Default::default()
        }
    }

    /// A [`policy_tx`] sponsored by `payer` whose `gas_limit + payer_auth` is
    /// exactly `reserved_gas`. `payer_auth` prices the payer signature's zero
    /// bytes, which change with every re-signing, so this re-signs with the
    /// previous `payer_auth` until the sum lands; varying the tip keeps
    /// deterministic signing from cycling between two prices.
    fn sponsored_policy_tx(
        signer: &PrivateKeySigner,
        payer: &PrivateKeySigner,
        nonce_key: U256,
        reserved_gas: u64,
    ) -> BasePooledTransaction {
        let mut gas_limit = POLICY_GAS_LIMIT;
        for max_priority_fee_per_gas in 0..32 {
            let tx = TxEip8130 {
                gas_limit,
                max_priority_fee_per_gas,
                payer: Some(payer.address()),
                ..policy_tx(nonce_key)
            };
            let transaction = sign_sponsored_8130(signer, payer, tx);
            let signed = transaction.as_eip8130().expect("EIP-8130 transaction");
            let payer_auth = IntrinsicGas::max_payer_auth_cost(signed).unwrap();
            assert!(payer_auth > 0);
            if gas_limit + payer_auth == reserved_gas {
                return transaction;
            }
            gas_limit = reserved_gas - payer_auth;
        }
        panic!("payer_auth did not settle for reserved gas {reserved_gas}");
    }

    fn invalid_kind(
        error: &reth_transaction_pool::error::PoolError,
    ) -> &InvalidPoolTransactionError {
        match &error.kind {
            PoolErrorKind::InvalidTransaction(kind) => kind,
            other => panic!("expected an invalid transaction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eip8130_routes_enforce_the_minimum_priority_fee() {
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            minimum_priority_fee: Some(10),
            ..Default::default()
        });
        let signer = signer();
        fund(&client, signer.address());

        for nonce_key in EIP8130_ROUTES {
            let with_tip = |max_priority_fee_per_gas| {
                sign_eoa_8130(
                    &signer,
                    TxEip8130 { max_priority_fee_per_gas, ..policy_tx(nonce_key) },
                )
            };
            let error = pool
                .add_transaction(TransactionOrigin::External, with_tip(9))
                .await
                .expect_err("a tip below the minimum is rejected");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::PriorityFeeBelowMinimum {
                        minimum_priority_fee: 10
                    }
                ),
                "nonce key {nonce_key}: {error:?}"
            );
            pool.add_transaction(TransactionOrigin::External, with_tip(10))
                .await
                .expect("a tip at the minimum is admitted");
        }
    }

    /// Reth applies the minimum priority fee to local submissions too, so a local
    /// EIP-8130 transaction is rejected exactly as a local EIP-1559 one is.
    #[tokio::test]
    async fn minimum_priority_fee_applies_to_local_submissions_of_every_type() {
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            minimum_priority_fee: Some(10),
            ..Default::default()
        });
        let signer = signer();
        fund(&client, signer.address());

        let eip1559 = signed_1559_with_fees(&signer, 0, POLICY_MAX_FEE, 9);
        let mut transactions = vec![eip1559];
        transactions.extend(EIP8130_ROUTES.map(|nonce_key| {
            sign_eoa_8130(
                &signer,
                TxEip8130 { max_priority_fee_per_gas: 9, ..policy_tx(nonce_key) },
            )
        }));
        for transaction in transactions {
            let hash = *transaction.hash();
            let error = pool
                .add_transaction(TransactionOrigin::Local, transaction)
                .await
                .expect_err("a local tip below the minimum is rejected");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::PriorityFeeBelowMinimum {
                        minimum_priority_fee: 10
                    }
                ),
                "transaction {hash}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn eip8130_routes_enforce_the_block_gas_limit() {
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            block_gas_limit: POLICY_GAS_LIMIT,
            ..Default::default()
        });
        let signer = signer();
        fund(&client, signer.address());

        for nonce_key in EIP8130_ROUTES {
            let with_gas =
                |gas_limit| sign_eoa_8130(&signer, TxEip8130 { gas_limit, ..policy_tx(nonce_key) });
            let error = pool
                .add_transaction(TransactionOrigin::External, with_gas(POLICY_GAS_LIMIT + 1))
                .await
                .expect_err("gas above the block gas limit is rejected");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::ExceedsGasLimit(gas, limit)
                        if *gas == POLICY_GAS_LIMIT + 1 && *limit == POLICY_GAS_LIMIT
                ),
                "nonce key {nonce_key}: {error:?}"
            );
            pool.add_transaction(TransactionOrigin::External, with_gas(POLICY_GAS_LIMIT))
                .await
                .expect("gas at the block gas limit is admitted");
        }
    }

    /// Execution reserves `gas_limit + payer_auth` against the block, so a
    /// sponsored transaction whose `gas_limit` alone fits is still rejected.
    #[tokio::test]
    async fn eip8130_block_gas_limit_includes_payer_authentication() {
        const SPONSORED_GAS_LIMIT: u64 = 1_000_000;
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            block_gas_limit: SPONSORED_GAS_LIMIT,
            ..Default::default()
        });
        let (signer, payer) = (signer(), signer());
        fund(&client, signer.address());
        fund(&client, payer.address());

        for nonce_key in EIP8130_ROUTES {
            let transaction = sponsored_eoa_8130(&signer, &payer, nonce_key, POLICY_MAX_FEE);
            let signed = transaction.as_eip8130().expect("EIP-8130 transaction");
            assert_eq!(signed.tx().gas_limit, SPONSORED_GAS_LIMIT);
            let payer_auth = IntrinsicGas::max_payer_auth_cost(signed).unwrap();
            assert!(payer_auth > 0);

            let error = pool
                .add_transaction(TransactionOrigin::External, transaction)
                .await
                .expect_err("gas_limit plus payer authentication exceeds the block gas limit");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::ExceedsGasLimit(gas, limit)
                        if *gas == SPONSORED_GAS_LIMIT + payer_auth
                            && *limit == SPONSORED_GAS_LIMIT
                ),
                "nonce key {nonce_key}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn eip8130_routes_enforce_the_max_tx_gas_limit() {
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            max_tx_gas_limit: Some(POLICY_GAS_LIMIT - 1),
            ..Default::default()
        });
        let signer = signer();
        fund(&client, signer.address());

        for nonce_key in EIP8130_ROUTES {
            let error = pool
                .add_transaction(
                    TransactionOrigin::External,
                    sign_eoa_8130(&signer, policy_tx(nonce_key)),
                )
                .await
                .expect_err("gas above the operator cap is rejected");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::MaxTxGasLimitExceeded(gas, cap)
                        if *gas == POLICY_GAS_LIMIT && *cap == POLICY_GAS_LIMIT - 1
                ),
                "nonce key {nonce_key}: {error:?}"
            );
        }
    }

    /// The operator cap bounds `gas_limit + payer_auth`, so a sponsored
    /// transaction whose `gas_limit` alone fits is still rejected.
    #[tokio::test]
    async fn eip8130_max_tx_gas_limit_includes_payer_authentication() {
        const CAP: u64 = POLICY_GAS_LIMIT;
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            max_tx_gas_limit: Some(CAP),
            ..Default::default()
        });
        let (signer, payer) = (signer(), signer());
        fund(&client, payer.address());

        for nonce_key in EIP8130_ROUTES {
            let transaction = sponsored_policy_tx(&signer, &payer, nonce_key, CAP + 1);
            let error = pool
                .add_transaction(TransactionOrigin::External, transaction)
                .await
                .expect_err("gas_limit plus payer authentication exceeds the operator cap");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::MaxTxGasLimitExceeded(gas, cap)
                        if *gas == CAP + 1 && *cap == CAP
                ),
                "nonce key {nonce_key}: {error:?}"
            );
            let transaction = sponsored_policy_tx(&signer, &payer, nonce_key, CAP);
            pool.add_transaction(TransactionOrigin::External, transaction)
                .await
                .expect("gas_limit plus payer authentication at the operator cap is admitted");
        }
    }

    /// The fee cap bounds `(gas_limit + payer_auth) * max_fee_per_gas`, so a
    /// sponsored transaction whose `gas_limit` alone fits is still rejected.
    #[tokio::test]
    async fn eip8130_fee_cap_includes_payer_authentication() {
        let tx_fee_cap = u128::from(POLICY_GAS_LIMIT) * POLICY_MAX_FEE;
        let (pool, client) =
            build_integration_pool_with_policies(PoolPolicies { tx_fee_cap, ..Default::default() });
        let (signer, payer) = (signer(), signer());
        fund(&client, payer.address());

        for nonce_key in EIP8130_ROUTES {
            let transaction = sponsored_policy_tx(&signer, &payer, nonce_key, POLICY_GAS_LIMIT + 1);
            let error = pool
                .add_transaction(TransactionOrigin::Local, transaction)
                .await
                .expect_err("the payer's maximum gas charge exceeds the fee cap");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::ExceedsFeeCap { max_tx_fee_wei, tx_fee_cap_wei }
                        if *max_tx_fee_wei == tx_fee_cap + POLICY_MAX_FEE
                            && *tx_fee_cap_wei == tx_fee_cap
                ),
                "nonce key {nonce_key}: {error:?}"
            );
            let transaction = sponsored_policy_tx(&signer, &payer, nonce_key, POLICY_GAS_LIMIT);
            pool.add_transaction(TransactionOrigin::Local, transaction)
                .await
                .expect("the payer's maximum gas charge at the fee cap is admitted");
        }
    }

    /// The RPC fee cap bounds the payer's maximum gas charge, and only for
    /// local submissions.
    #[tokio::test]
    async fn eip8130_routes_enforce_the_fee_cap_for_local_submissions() {
        let max_tx_fee = u128::from(POLICY_GAS_LIMIT) * POLICY_MAX_FEE;
        let (pool, client) = build_integration_pool_with_policies(PoolPolicies {
            tx_fee_cap: max_tx_fee - 1,
            ..Default::default()
        });
        let signer = signer();
        fund(&client, signer.address());

        for nonce_key in EIP8130_ROUTES {
            let transaction = sign_eoa_8130(&signer, policy_tx(nonce_key));
            let error = pool
                .add_transaction(TransactionOrigin::Local, transaction.clone())
                .await
                .expect_err("a local submission above the fee cap is rejected");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::ExceedsFeeCap { max_tx_fee_wei, tx_fee_cap_wei }
                        if *max_tx_fee_wei == max_tx_fee && *tx_fee_cap_wei == max_tx_fee - 1
                ),
                "nonce key {nonce_key}: {error:?}"
            );
            pool.add_transaction(TransactionOrigin::External, transaction)
                .await
                .expect("the fee cap does not apply to external transactions");
        }
    }

    /// Both routes recheck the block gas limit at insert against the pool's
    /// block info, which can move after validation.
    #[tokio::test]
    async fn eip8130_routes_recheck_the_block_gas_limit_at_insert() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        pool.set_block_info(BlockInfo {
            block_gas_limit: POLICY_GAS_LIMIT - 1,
            ..pool.block_info()
        });

        for nonce_key in EIP8130_ROUTES {
            let error = pool
                .add_transaction(
                    TransactionOrigin::External,
                    sign_eoa_8130(&signer, policy_tx(nonce_key)),
                )
                .await
                .expect_err("gas above the pool's block gas limit is rejected at insert");
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::ExceedsGasLimit(gas, limit)
                        if *gas == POLICY_GAS_LIMIT && *limit == POLICY_GAS_LIMIT - 1
                ),
                "nonce key {nonce_key}: {error:?}"
            );
        }
    }

    /// The insert-time recheck bounds `gas_limit + payer_auth`, as validation does,
    /// so a sponsored transaction whose `gas_limit` alone fits the moved block gas
    /// limit is still rejected.
    #[tokio::test]
    async fn eip8130_insert_recheck_includes_payer_authentication() {
        let (pool, client) = build_integration_pool();
        let (signer, payer) = (signer(), signer());
        fund(&client, payer.address());
        pool.set_block_info(BlockInfo { block_gas_limit: POLICY_GAS_LIMIT, ..pool.block_info() });

        for nonce_key in EIP8130_ROUTES {
            let transaction = sponsored_policy_tx(&signer, &payer, nonce_key, POLICY_GAS_LIMIT + 1);
            let error =
                pool.add_transaction(TransactionOrigin::External, transaction).await.expect_err(
                    "gas_limit plus payer authentication exceeds the pool's block gas limit",
                );
            assert!(
                matches!(
                    invalid_kind(&error),
                    InvalidPoolTransactionError::ExceedsGasLimit(gas, limit)
                        if *gas == POLICY_GAS_LIMIT + 1 && *limit == POLICY_GAS_LIMIT
                ),
                "nonce key {nonce_key}: {error:?}"
            );
            let transaction = sponsored_policy_tx(&signer, &payer, nonce_key, POLICY_GAS_LIMIT);
            pool.add_transaction(TransactionOrigin::External, transaction)
                .await
                .expect("gas_limit plus payer authentication at the block gas limit is admitted");
        }
    }

    #[tokio::test]
    async fn stale_sidecar_replacement_restores_original_transaction() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let original = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        let replacement = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_250);
        let replacement_hash = *replacement.hash();
        let mut replacement_events =
            pool.sidecar.listeners.write().subscribe_hash(replacement_hash).0;
        let balance_epoch = pool.sidecar.pool.read().balance_epoch();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::Local, replacement).await;
        pool.validator().validator().clear_limit_class_cache();

        let error = pool
            .add_validated_lane_transaction(
                PoolStore::Sidecar,
                validated,
                TransactionOrigin::Local,
                balance_epoch,
            )
            .expect_err("stale classification must reject the replacement");
        assert!(error.to_string().contains("classification changed during validation"));
        assert!(pool.get(&replacement_hash).is_none());
        assert!(pool.get(&original_hash).is_some());
        assert!(pool.guard.read().contains(&original_hash));
        assert!(!pool.guard.read().contains(&replacement_hash));
        assert!(matches!(replacement_events.next().await, Some(TransactionEvent::Discarded)));
    }

    #[tokio::test]
    async fn rolled_back_insert_still_announces_the_anchored_lane() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let queued = self_paid_eoa_8130(&signer, U256::from(1), 1, 0, 1_000);
        let queued_hash = *queued.hash();
        pool.add_transaction(TransactionOrigin::Local, queued).await.unwrap();
        let mut queued_events = pool.sidecar.listeners.write().subscribe_hash(queued_hash).0;

        let successor = self_paid_eoa_8130(&signer, U256::from(1), 2, 0, 1_000);
        let balance_epoch = pool.sidecar.pool.read().balance_epoch();
        let validated = match pool
            .validator()
            .validate_transaction(TransactionOrigin::Local, successor)
            .await
        {
            TransactionValidationOutcome::Valid {
                balance,
                bytecode_hash,
                transaction,
                propagate,
                authorities,
                ..
            } => TransactionValidationOutcome::Valid {
                balance,
                state_nonce: 1,
                bytecode_hash,
                transaction,
                propagate,
                authorities,
            },
            other => panic!("successor must validate: {other:?}"),
        };
        pool.validator().validator().clear_limit_class_cache();

        pool.add_validated_lane_transaction(
            PoolStore::Sidecar,
            validated,
            TransactionOrigin::Local,
            balance_epoch,
        )
        .expect_err("stale classification must roll back the insert");

        let pending = pool.sidecar.pool.read().pending_transactions();
        assert!(pending.iter().any(|transaction| *transaction.hash() == queued_hash));
        assert!(matches!(
            queued_events.next().now_or_never(),
            Some(Some(TransactionEvent::Pending))
        ));
    }

    #[tokio::test]
    async fn insert_discarded_by_eviction_still_announces_the_anchored_lane() {
        let config =
            PoolConfig { pending_limit: SubPoolLimit::new(2, usize::MAX), ..PoolConfig::default() };
        let (pool, client) = build_integration_pool_with_config(config);
        let lane_signer = signer();
        let other_signer = signer();
        fund(&client, lane_signer.address());
        fund(&client, other_signer.address());

        let queued = self_paid_eoa_8130(&lane_signer, U256::from(1), 1, 0, 1_000);
        let queued_hash = *queued.hash();
        pool.add_transaction(TransactionOrigin::External, queued).await.unwrap();
        let other = self_paid_eoa_8130(&other_signer, U256::from(1), 0, 0, 5_000);
        pool.add_transaction(TransactionOrigin::External, other).await.unwrap();
        let mut queued_events = pool.sidecar.listeners.write().subscribe_hash(queued_hash).0;

        let successor = self_paid_eoa_8130(&lane_signer, U256::from(1), 2, 0, 1_000);
        let balance_epoch = pool.sidecar.pool.read().balance_epoch();
        let validated = match pool
            .validator()
            .validate_transaction(TransactionOrigin::External, successor)
            .await
        {
            TransactionValidationOutcome::Valid {
                balance,
                bytecode_hash,
                transaction,
                propagate,
                authorities,
                ..
            } => TransactionValidationOutcome::Valid {
                balance,
                state_nonce: 1,
                bytecode_hash,
                transaction,
                propagate,
                authorities,
            },
            other => panic!("successor must validate: {other:?}"),
        };

        let error = pool
            .add_validated_lane_transaction(
                PoolStore::Sidecar,
                validated,
                TransactionOrigin::External,
                balance_epoch,
            )
            .expect_err("the cheapest tail is the inserted transaction");
        assert!(matches!(error.kind, PoolErrorKind::DiscardedOnInsert));

        let pending = pool.sidecar.pool.read().pending_transactions();
        assert!(pending.iter().any(|transaction| *transaction.hash() == queued_hash));
        assert!(matches!(
            queued_events.next().now_or_never(),
            Some(Some(TransactionEvent::Pending))
        ));
    }

    #[tokio::test]
    async fn exact_state_diff_and_feed_gap_remove_sidecar_members() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let keyed = self_paid_eoa_8130(&signer, U256::from(2), 0, 0, 1_000);
        let keyed_hash = *keyed.hash();
        pool.add_transaction(TransactionOrigin::Local, keyed).await.unwrap();
        let pooled = pool.get(&keyed_hash).unwrap();
        let (address, slot) = pooled
            .transaction
            .watch_set()
            .unwrap()
            .iter()
            .find_map(|key| match key {
                InvalidationKey::Slot { address, slot } => Some((*address, *slot)),
                _ => None,
            })
            .expect("a 2D nonce channel must expose an exact nonce-slot dependency");

        let removed = pool.apply_state_diff(&[AccountStateDiff {
            address,
            changed_slots: vec![(slot, U256::from(1))],
            ..Default::default()
        }]);
        assert_eq!(removed.len(), 1);
        assert_eq!(*removed[0].hash(), keyed_hash);
        assert!(pool.get(&keyed_hash).is_none());
        assert!(!pool.guard.read().contains(&keyed_hash));

        let channel = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        let channel_hash = *channel.hash();
        pool.add_transaction(TransactionOrigin::Local, channel).await.unwrap();
        assert!(pool.guard.read().contains(&channel_hash));

        let removed = pool.invalidate_all_tracked_transactions(InvalidationCause::Reorg);
        assert_eq!(removed.len(), 1);
        assert_eq!(*removed[0].hash(), channel_hash);
        assert!(pool.get(&channel_hash).is_none());
        assert!(pool.guard.read().is_empty());
    }

    /// Including a lane head advances the nonce its successors watch. Canonical
    /// invalidation drops only what the advance leaves behind, on both the
    /// sidecar (2D channel slot) and protocol (key-0 account nonce) routes.
    #[tokio::test]
    async fn canonical_nonce_advance_keeps_lane_successors() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let channel = U256::from(2);
        let lane: Vec<_> = (0..2)
            .map(|sequence| self_paid_eoa_8130(&signer, channel, sequence, 0, 1_000))
            .collect();
        let protocol: Vec<_> = (0..2)
            .map(|sequence| self_paid_eoa_8130(&signer, U256::ZERO, sequence, 0, 1_000))
            .collect();
        for transaction in lane.iter().chain(&protocol) {
            pool.add_transaction(TransactionOrigin::Local, transaction.clone()).await.unwrap();
        }
        let mut successor_events = pool.sidecar.listeners.write().subscribe_hash(*lane[1].hash()).0;
        let slot = NonceManagerStorage::nonce_slot(signer.address(), channel).unwrap();

        let removed = pool.apply_state_diff(&[
            AccountStateDiff {
                address: NonceManagerStorage::ADDRESS,
                changed_slots: vec![(B256::from(slot), U256::from(1))],
                ..Default::default()
            },
            AccountStateDiff { address: signer.address(), nonce: Some(1), ..Default::default() },
        ]);

        let mut removed: Vec<_> = removed.iter().map(|transaction| *transaction.hash()).collect();
        removed.sort_unstable();
        let mut expected = vec![*lane[0].hash(), *protocol[0].hash()];
        expected.sort_unstable();
        assert_eq!(removed, expected);
        for kept in [&lane[1], &protocol[1]] {
            assert!(pool.get(kept.hash()).is_some());
            assert!(pool.guard.read().contains(kept.hash()));
        }
        let pending: Vec<_> = pool
            .sidecar
            .pool
            .read()
            .pending_transactions()
            .iter()
            .map(|transaction| *transaction.hash())
            .collect();
        assert!(pending.contains(lane[1].hash()));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), successor_events.next()).await.is_err(),
            "a successor that stays pending is not re-announced"
        );
    }

    #[tokio::test]
    async fn stale_nonce_diff_does_not_rewind_a_lane() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let channel = U256::from(2);
        let lane: Vec<_> = (0..3)
            .map(|sequence| self_paid_eoa_8130(&signer, channel, sequence, 0, 1_000))
            .collect();
        for transaction in &lane {
            pool.add_transaction(TransactionOrigin::Local, transaction.clone()).await.unwrap();
        }
        let slot = B256::from(NonceManagerStorage::nonce_slot(signer.address(), channel).unwrap());
        let nonce_diff = |value: u64| AccountStateDiff {
            address: NonceManagerStorage::ADDRESS,
            changed_slots: vec![(slot, U256::from(value))],
            ..Default::default()
        };
        pool.apply_state_diff(&[nonce_diff(2)]);
        let mut head_events = pool.sidecar.listeners.write().subscribe_hash(*lane[2].hash()).0;

        let removed = pool.apply_state_diff(&[nonce_diff(1)]);

        assert!(removed.is_empty());
        let pending: Vec<_> = pool
            .sidecar
            .pool
            .read()
            .pending_transactions()
            .iter()
            .map(|transaction| *transaction.hash())
            .collect();
        assert_eq!(pending, vec![*lane[2].hash()]);
        assert!(head_events.next().now_or_never().is_none(), "the head must not be re-queued");
    }

    #[tokio::test]
    async fn canonical_maintenance_prunes_mined_nonce_free_and_releases_guard() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let transaction = self_paid_eoa_8130(
            &signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            INTEGRATION_POOL_NOW_MS + Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW,
            1_000,
        );
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        assert!(pool.sidecar.pool.read().contains(&hash));
        assert!(pool.guard.read().contains(&hash));

        let block = SealedBlock::seal_slow(BaseBlock {
            header: alloy_consensus::Header { timestamp: 1, ..Default::default() },
            body: Default::default(),
        });
        pool.on_canonical_state_change(CanonicalStateUpdate {
            new_tip: &block,
            pending_block_base_fee: 0,
            pending_block_blob_fee: None,
            changed_accounts: Vec::new(),
            mined_transactions: vec![hash],
            update_kind: PoolUpdateKind::Commit,
        });

        assert!(pool.get(&hash).is_none());
        assert!(!pool.sidecar.pool.read().contains(&hash));
        assert!(!pool.guard.read().contains(&hash));
    }

    #[tokio::test]
    async fn account_update_releases_guard_for_protocol_discard() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let transaction = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        assert!(pool.guard.read().contains(&hash));

        pool.update_accounts(vec![ChangedAccount {
            address: signer.address(),
            nonce: 1,
            balance: U256::MAX,
        }]);

        assert!(pool.get(&hash).is_none());
        assert!(!pool.guard.read().contains(&hash));
    }

    fn is_pending(pool: &IntegrationPool, hash: TxHash) -> bool {
        pool.pending_transactions().iter().any(|transaction| *transaction.hash() == hash)
    }

    #[tokio::test]
    async fn sponsored_key0_transaction_is_not_demoted_by_sender_account_changes() {
        let (pool, client) = build_integration_pool();
        let [sender, payer] = std::array::from_fn(|_| signer());
        fund(&client, payer.address());
        let transaction = sponsored_eoa_8130(&sender, &payer, U256::ZERO, 1_000);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        assert!(is_pending(&pool, hash));

        pool.update_accounts(vec![ChangedAccount {
            address: sender.address(),
            nonce: 0,
            balance: U256::from(1),
        }]);
        assert!(is_pending(&pool, hash), "the unfunded sender does not pay for a sponsored tx");
        assert!(pool.best_transactions().any(|transaction| *transaction.hash() == hash));

        pool.update_accounts(vec![ChangedAccount {
            address: payer.address(),
            nonce: 0,
            balance: U256::from(1),
        }]);
        assert!(pool.get(&hash).is_none(), "the payer's balance still bounds a sponsored tx");
    }

    /// More of a payer's own transactions than reth's EIP-7702 delegation
    /// slot limit allows a delegated account.
    fn over_delegation_slot_limit() -> u64 {
        PoolConfig::default().max_inflight_delegated_slot_limit as u64 + 1
    }

    #[tokio::test]
    async fn sponsoring_a_key0_transaction_does_not_limit_the_payers_own_transactions() {
        let (pool, client) = build_integration_pool();
        let [sender, payer] = std::array::from_fn(|_| signer());
        fund(&client, payer.address());
        let sponsored = sponsored_eoa_8130(&sender, &payer, U256::ZERO, 1_000);
        pool.add_transaction(TransactionOrigin::External, sponsored).await.unwrap();

        for nonce in 0..over_delegation_slot_limit() {
            let result =
                pool.add_transaction(TransactionOrigin::External, signed_1559(&payer, nonce)).await;
            assert!(result.is_ok(), "payer transaction {nonce} was rejected: {result:?}");
        }
    }

    #[tokio::test]
    async fn a_payer_with_pending_transactions_can_sponsor_a_key0_transaction() {
        let (pool, client) = build_integration_pool();
        let [sender, payer] = std::array::from_fn(|_| signer());
        fund(&client, payer.address());
        for nonce in 0..over_delegation_slot_limit() {
            pool.add_transaction(TransactionOrigin::External, signed_1559(&payer, nonce))
                .await
                .unwrap();
        }

        let sponsored = sponsored_eoa_8130(&sender, &payer, U256::ZERO, 1_000);
        let hash = *sponsored.hash();
        let result = pool.add_transaction(TransactionOrigin::External, sponsored).await;
        assert!(result.is_ok(), "the sponsored transaction was rejected: {result:?}");
        assert!(is_pending(&pool, hash));
    }

    #[tokio::test]
    async fn self_paid_key0_transaction_leaves_pending_when_sender_balance_drops() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        fund(&client, sender.address());
        let transaction = self_paid_eoa_8130(&sender, U256::ZERO, 0, 0, 1_000);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        assert!(is_pending(&pool, hash));

        pool.update_accounts(vec![ChangedAccount {
            address: sender.address(),
            nonce: 0,
            balance: U256::from(1),
        }]);
        assert!(!is_pending(&pool, hash));
        assert!(pool.best_transactions().all(|transaction| *transaction.hash() != hash));
    }

    const TIPPING_MAX_FEE_PER_GAS: u128 = 1_000;
    const TIPPING_GAS_LIMIT: u64 = 1_000_000;
    const TIPPING_NONCE_KEY: U256 = U256::from_limbs([7, 0, 0, 0]);

    /// The most a [`tipping_eoa_8130`] transaction can charge in fees.
    fn tipping_fees() -> U256 {
        U256::from(TIPPING_GAS_LIMIT) * U256::from(TIPPING_MAX_FEE_PER_GAS)
    }

    /// A key-0 EIP-8130 transaction declaring `tip` to the sequencer fee vault.
    fn tipping_eoa_8130(
        signer: &PrivateKeySigner,
        payer: Option<&PrivateKeySigner>,
        nonce_sequence: u64,
        tip: U256,
    ) -> BasePooledTransaction {
        tipping_8130(signer, payer, U256::ZERO, nonce_sequence, 0, tip)
    }

    /// A tipping finite-channel sidecar transaction on [`TIPPING_NONCE_KEY`].
    fn tipping_channel_8130(
        signer: &PrivateKeySigner,
        payer: Option<&PrivateKeySigner>,
        nonce_sequence: u64,
        tip: U256,
    ) -> BasePooledTransaction {
        tipping_8130(signer, payer, TIPPING_NONCE_KEY, nonce_sequence, 0, tip)
    }

    /// A tipping nonce-free sidecar transaction; `expiry_offset_ms` keeps
    /// each one a distinct sidecar member.
    fn tipping_nonce_free_8130(
        signer: &PrivateKeySigner,
        expiry_offset_ms: u64,
        tip: U256,
    ) -> BasePooledTransaction {
        tipping_8130(
            signer,
            None,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            INTEGRATION_POOL_NOW_MS + expiry_offset_ms + 1,
            tip,
        )
    }

    fn tipping_8130(
        signer: &PrivateKeySigner,
        payer: Option<&PrivateKeySigner>,
        nonce_key: U256,
        nonce_sequence: u64,
        valid_before: u64,
        tip: U256,
    ) -> BasePooledTransaction {
        tipping_8130_with_fees(
            signer,
            payer,
            nonce_key,
            nonce_sequence,
            valid_before,
            tip,
            (0, TIPPING_MAX_FEE_PER_GAS),
        )
    }

    /// [`tipping_8130`] with explicit `(max_priority_fee_per_gas, max_fee_per_gas)`.
    fn tipping_8130_with_fees(
        signer: &PrivateKeySigner,
        payer: Option<&PrivateKeySigner>,
        nonce_key: U256,
        nonce_sequence: u64,
        valid_before: u64,
        tip: U256,
        (max_priority_fee_per_gas, max_fee_per_gas): (u128, u128),
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit: TIPPING_GAS_LIMIT,
            account_changes: Vec::new(),
            calls: vec![vec![Call {
                to: Predeploys::SEQUENCER_FEE_VAULT,
                value: tip,
                data: Bytes::new(),
            }]],
            metadata: Bytes::new(),
            payer: payer.map(PrivateKeySigner::address),
        };
        let sender_auth = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let payer_auth = payer.map_or_else(Vec::new, |payer| {
            let payer_sig =
                payer.sign_hash_sync(&tx.payer_signature_hash(signer.address())).unwrap();
            let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
            payer_auth.extend_from_slice(&payer_sig.as_bytes());
            payer_auth
        });
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from(sender_auth.as_bytes().to_vec()),
            Bytes::from(payer_auth),
        );
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn set_balance(pool: &IntegrationPool, address: Address, balance: U256) {
        pool.update_accounts(vec![ChangedAccount { address, nonce: 0, balance }]);
    }

    fn is_best(pool: &IntegrationPool, hash: TxHash) -> bool {
        pool.best_transactions().any(|transaction| *transaction.hash() == hash)
    }

    #[tokio::test]
    async fn one_balance_backs_only_one_full_balance_coinbase_tip() {
        let mut config = PoolConfig::default();
        config.pending_limit.max_txs = 2;
        let (pool, client) = build_integration_pool_with_config(config);
        let [attacker, honest] = std::array::from_fn(|_| signer());
        let balance = U256::from(10) * tipping_fees();
        client.add_account(attacker.address(), ExtendedAccount::new(0, balance));
        fund(&client, honest.address());

        let honest_tx = signed_1559_with_fees(&honest, 0, 1_000, 1);
        let honest_hash = *honest_tx.hash();
        pool.add_transaction(TransactionOrigin::External, honest_tx).await.unwrap();

        let full_balance_tip = balance - tipping_fees();
        let mut hashes = Vec::new();
        for nonce in 0..4 {
            let transaction = tipping_eoa_8130(&attacker, None, nonce, full_balance_tip);
            hashes.push(*transaction.hash());
            let _ = pool.add_transaction(TransactionOrigin::External, transaction).await;
        }

        assert!(is_pending(&pool, honest_hash), "unbacked tips must not evict an honest tx");
        assert!(is_best(&pool, honest_hash));
        assert!(is_pending(&pool, hashes[0]), "the balance backs the first tip");
        for hash in &hashes[1..] {
            assert!(!is_pending(&pool, *hash), "an over-committed tip must not rank");
            assert!(!is_best(&pool, *hash));
        }
    }

    #[tokio::test]
    async fn only_the_backed_prefix_of_coinbase_tips_is_pending() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        // The balance covers all three transactions' fees but only the first tip.
        let tip = U256::from(2) * tipping_fees();
        client.add_account(
            sender.address(),
            ExtendedAccount::new(0, U256::from(3) * tipping_fees() + tip),
        );

        let mut hashes = Vec::new();
        for nonce in 0..3 {
            let transaction = tipping_eoa_8130(&sender, None, nonce, tip);
            hashes.push(*transaction.hash());
            pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        }

        assert!(is_pending(&pool, hashes[0]));
        for hash in &hashes[1..] {
            assert!(pool.get(hash).is_some());
            assert!(!is_pending(&pool, *hash));
        }
    }

    #[tokio::test]
    async fn coinbase_tip_ranks_only_while_the_sender_balance_backs_it() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        let tip = tipping_fees();
        let backed = tipping_fees() + tip;
        client.add_account(sender.address(), ExtendedAccount::new(0, backed));
        let transaction = tipping_eoa_8130(&sender, None, 0, tip);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        assert!(is_pending(&pool, hash));

        set_balance(&pool, sender.address(), backed - U256::from(1));
        assert!(pool.get(&hash).is_some(), "the fees are still covered");
        assert!(!is_pending(&pool, hash));
        assert!(!is_best(&pool, hash));

        set_balance(&pool, sender.address(), backed);
        assert!(is_pending(&pool, hash));
    }

    /// Isthmus L1 attributes calldata with non-zero L1 data and operator fee
    /// parameters.
    const ISTHMUS_L1_INFO_CALLDATA: [u8; 176] = alloy_primitives::hex!(
        "098999be00000558000c5fc500000000000000030000000067a9f765"
        "0000000000000029000000000000000000000000000000000000000000000000"
        "00000000006a6d090000000000000000000000000000000000000000000000000000000000000001"
        "72fcc8e8886636bdbe96ba0e4baab67ea7e7811633f52b52e8cf7a5123213b6f"
        "000000000000000000000000d3f2c5afb2d76f5579f326b0cd7da5f5a4126c35"
        "00004e2000000000000001f4"
    );

    fn l1_info_deposit() -> BaseTransactionSigned {
        TxDeposit {
            to: TxKind::Create,
            input: Bytes::from_static(&ISTHMUS_L1_INFO_CALLDATA),
            ..Default::default()
        }
        .into()
    }

    /// The L1 data and operator fees a self-paid transaction prepays under
    /// [`ISTHMUS_L1_INFO_CALLDATA`].
    fn l1_and_operator_fees(pool: &IntegrationPool, transaction: &BasePooledTransaction) -> U256 {
        let mut l1_block_info = base_execution_evm::parse_l1_info(&ISTHMUS_L1_INFO_CALLDATA)
            .expect("valid L1 info fixture");
        let spec = BaseSpecId::from_timestamp(
            pool.validator().validator().chain_spec(),
            INTEGRATION_POOL_NOW_SECS,
        );
        l1_block_info.tx_cost(transaction.encoded_2718(), U256::from(transaction.gas_limit()), spec)
    }

    /// A self-paid tip is prepaid after the L1 data and operator fees, so a
    /// balance of exactly gas plus the tip does not back it.
    #[tokio::test]
    async fn self_paid_coinbase_tip_is_backed_only_after_l1_and_operator_fees() {
        let (pool, client) = build_integration_pool_with_l1_fees();
        let [short, funded] = std::array::from_fn(|_| signer());
        let tip = U256::from(10u64).pow(U256::from(18u64));

        let short_tx = tipping_eoa_8130(&short, None, 0, tip);
        let short_hash = *short_tx.hash();
        assert!(!l1_and_operator_fees(&pool, &short_tx).is_zero(), "fixture must charge fees");
        client.add_account(short.address(), ExtendedAccount::new(0, tipping_fees() + tip));
        pool.add_transaction(TransactionOrigin::External, short_tx).await.unwrap();
        assert!(pool.get(&short_hash).is_some(), "the balance still covers the fees");
        assert!(!is_pending(&pool, short_hash), "gas plus the tip must not back the tip");
        assert!(!is_best(&pool, short_hash));

        let funded_tx = tipping_eoa_8130(&funded, None, 0, tip);
        let funded_hash = *funded_tx.hash();
        let balance = tipping_fees() + l1_and_operator_fees(&pool, &funded_tx) + tip;
        client.add_account(funded.address(), ExtendedAccount::new(0, balance));
        pool.add_transaction(TransactionOrigin::External, funded_tx).await.unwrap();
        assert!(is_pending(&pool, funded_hash));
    }

    #[tokio::test]
    async fn sponsored_coinbase_tip_is_backed_by_the_sender() {
        let (pool, client) = build_integration_pool();
        let [unfunded, funded, payer] = std::array::from_fn(|_| signer());
        fund(&client, payer.address());
        let tip = tipping_fees();
        client.add_account(funded.address(), ExtendedAccount::new(0, tip));

        let unbacked = tipping_eoa_8130(&unfunded, Some(&payer), 0, tip);
        let unbacked_hash = *unbacked.hash();
        pool.add_transaction(TransactionOrigin::External, unbacked).await.unwrap();
        assert!(pool.get(&unbacked_hash).is_some());
        assert!(!is_pending(&pool, unbacked_hash), "the payer does not back the sender's tip");

        let backed = tipping_eoa_8130(&funded, Some(&payer), 0, tip);
        let backed_hash = *backed.hash();
        pool.add_transaction(TransactionOrigin::External, backed).await.unwrap();
        assert!(is_pending(&pool, backed_hash));

        set_balance(&pool, funded.address(), tip - U256::from(1));
        assert!(!is_pending(&pool, backed_hash));
        assert!(!is_best(&pool, backed_hash));
    }

    /// Adds a backed self-paid tx0 below the honest transaction's rank, then
    /// tx1 declaring `tx1_tip(c0)` where `c0` is tx0's cost, and asserts tx1
    /// neither pends nor outranks the honest transaction.
    async fn assert_tip_cannot_wrap_cumulative_cost(tx1_tip: impl FnOnce(U256) -> U256) {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, _) = setup_tip_race(&pool, &client, &attacker).await;
        let tx0_tip = U256::from(3);
        let tx0 = tipping_eoa_8130(&attacker, None, 0, tx0_tip);
        let tx0_hash = *tx0.hash();
        pool.add_transaction(TransactionOrigin::External, tx0).await.unwrap();
        assert!(is_pending(&pool, tx0_hash));

        let tx1 = tipping_eoa_8130(&attacker, None, 1, tx1_tip(tipping_fees() + tx0_tip));
        let tx1_hash = *tx1.hash();
        let _ = pool.add_transaction(TransactionOrigin::External, tx1).await;

        assert!(!is_pending(&pool, tx1_hash), "an unbacked tip must not wrap into pending");
        assert!(!is_best(&pool, tx1_hash));
        assert_eq!(best_hashes(&pool).first(), Some(&honest_hash));
    }

    #[tokio::test]
    async fn maximal_coinbase_tip_cannot_wrap_the_cumulative_cost() {
        assert_tip_cannot_wrap_cumulative_cost(|_| U256::MAX).await;
    }

    #[tokio::test]
    async fn exact_wrapping_coinbase_tip_cannot_wrap_the_cumulative_cost() {
        assert_tip_cannot_wrap_cumulative_cost(|c0| U256::ZERO.wrapping_sub(c0) + U256::from(1))
            .await;
    }

    fn best_hashes(pool: &IntegrationPool) -> Vec<TxHash> {
        pool.best_transactions().map(|transaction| *transaction.hash()).collect()
    }

    /// Funds `attacker` with ten times the tipping fees, adds an honest
    /// EIP-1559 transaction with priority fee 1, and returns its hash and the
    /// attacker's balance.
    async fn setup_tip_race(
        pool: &IntegrationPool,
        client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        attacker: &PrivateKeySigner,
    ) -> (TxHash, U256) {
        let honest = signer();
        fund(client, honest.address());
        let balance = U256::from(10) * tipping_fees();
        client.add_account(attacker.address(), ExtendedAccount::new(0, balance));
        let honest_tx = signed_1559_with_fees(&honest, 0, 1_000, 1);
        let honest_hash = *honest_tx.hash();
        pool.add_transaction(TransactionOrigin::External, honest_tx).await.unwrap();
        (honest_hash, balance)
    }

    #[tokio::test]
    async fn sidecar_lane_balance_backs_only_one_full_balance_coinbase_tip() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, balance) = setup_tip_race(&pool, &client, &attacker).await;
        let full_balance_tip = balance - tipping_fees();

        let backed = tipping_channel_8130(&attacker, None, 0, full_balance_tip);
        let backed_hash = *backed.hash();
        pool.add_transaction(TransactionOrigin::External, backed).await.unwrap();
        assert_eq!(best_hashes(&pool)[0], backed_hash, "a backed tip ranks by the tip");

        for nonce in 1..3 {
            let transaction = tipping_channel_8130(&attacker, None, nonce, full_balance_tip);
            pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        }
        assert!(is_pending(&pool, backed_hash));
        assert_eq!(best_hashes(&pool)[0], honest_hash, "over-committed tips rank by priority fee");
    }

    #[tokio::test]
    async fn nonce_free_balance_backs_only_one_full_balance_coinbase_tip() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, balance) = setup_tip_race(&pool, &client, &attacker).await;
        let full_balance_tip = balance - tipping_fees();

        let backed = tipping_nonce_free_8130(&attacker, 0, full_balance_tip);
        let backed_hash = *backed.hash();
        pool.add_transaction(TransactionOrigin::External, backed).await.unwrap();
        assert_eq!(best_hashes(&pool)[0], backed_hash, "a backed tip ranks by the tip");

        for offset in 1..3 {
            let transaction = tipping_nonce_free_8130(&attacker, offset, full_balance_tip);
            pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        }
        assert!(is_pending(&pool, backed_hash));
        assert_eq!(best_hashes(&pool)[0], honest_hash, "over-committed tips rank by priority fee");
    }

    #[tokio::test]
    async fn sidecar_coinbase_tip_backing_follows_sender_balance_updates() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, balance) = setup_tip_race(&pool, &client, &attacker).await;
        let tip = balance - tipping_fees();
        let transaction = tipping_channel_8130(&attacker, None, 0, tip);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        assert_eq!(best_hashes(&pool)[0], hash);

        set_balance(&pool, attacker.address(), balance - U256::from(1));
        assert!(pool.get(&hash).is_some(), "the fees are still covered");
        assert_eq!(best_hashes(&pool)[0], honest_hash, "a balance drop un-backs the tip");

        pool.apply_state_diff(&[AccountStateDiff {
            balance: Some(balance),
            ..AccountStateDiff::new(attacker.address())
        }]);
        assert_eq!(best_hashes(&pool)[0], hash, "a canonical balance restore re-backs the tip");
    }

    /// Validates `transaction`, applies `balance` for its sender as a newer
    /// account update, then inserts the validated transaction.
    async fn insert_after_balance_drop(
        pool: &IntegrationPool,
        transaction: BasePooledTransaction,
        balance: U256,
    ) {
        let sender = transaction.sender();
        let balance_epoch = pool.sidecar.pool.read().balance_epoch();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::External, transaction).await;
        set_balance(pool, sender, balance);
        pool.add_validated_lane_transaction(
            PoolStore::Sidecar,
            validated,
            TransactionOrigin::External,
            balance_epoch,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn balance_drop_during_validation_of_a_first_sidecar_tip_is_not_overwritten() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, balance) = setup_tip_race(&pool, &client, &attacker).await;
        let tip = balance - tipping_fees();

        let transaction = tipping_channel_8130(&attacker, None, 0, tip);
        let hash = *transaction.hash();
        insert_after_balance_drop(&pool, transaction, balance - U256::from(1)).await;
        assert!(is_pending(&pool, hash));
        assert_eq!(best_hashes(&pool)[0], honest_hash, "the stale balance must not back the tip");
    }

    #[tokio::test]
    async fn balance_drop_during_validation_of_a_further_sidecar_tip_is_not_overwritten() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, balance) = setup_tip_race(&pool, &client, &attacker).await;
        let untipped = tipping_nonce_free_8130(&attacker, 0, U256::ZERO);
        pool.add_transaction(TransactionOrigin::External, untipped).await.unwrap();
        let tip = balance - U256::from(2) * tipping_fees();

        let transaction = tipping_channel_8130(&attacker, None, 0, tip);
        let hash = *transaction.hash();
        insert_after_balance_drop(&pool, transaction, balance - U256::from(1)).await;
        assert!(is_pending(&pool, hash));
        assert_eq!(best_hashes(&pool)[0], honest_hash, "the stale balance must not back the tip");

        set_balance(&pool, attacker.address(), balance);
        assert_eq!(best_hashes(&pool)[0], hash, "a current balance backs the tip again");
    }

    #[tokio::test]
    async fn replacing_an_oversized_sidecar_tip_keeps_it_unbacked() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, _) = setup_tip_race(&pool, &client, &attacker).await;
        let original = tipping_channel_8130(&attacker, None, 0, U256::MAX);
        pool.add_transaction(TransactionOrigin::External, original).await.unwrap();

        let bumped_fees = (0, TIPPING_MAX_FEE_PER_GAS * 2);
        let replacement = tipping_8130_with_fees(
            &attacker,
            None,
            TIPPING_NONCE_KEY,
            0,
            0,
            U256::MAX,
            bumped_fees,
        );
        let replacement_hash = *replacement.hash();
        let outcome = pool.add_transaction(TransactionOrigin::External, replacement).await.unwrap();
        assert_eq!(outcome.hash, replacement_hash);
        assert!(is_pending(&pool, replacement_hash));
        assert_eq!(best_hashes(&pool)[0], honest_hash, "an impossible tip must not rank");
    }

    #[tokio::test]
    async fn removing_one_of_two_oversized_sidecar_tips_keeps_the_other_unbacked() {
        let (pool, client) = build_integration_pool();
        let attacker = signer();
        let (honest_hash, _) = setup_tip_race(&pool, &client, &attacker).await;
        let [kept, removed] =
            [0, 1].map(|offset| tipping_nonce_free_8130(&attacker, offset, U256::MAX));
        let removed_hash = *removed.hash();
        pool.add_transaction(TransactionOrigin::External, kept).await.unwrap();
        pool.add_transaction(TransactionOrigin::External, removed).await.unwrap();

        assert_eq!(pool.remove_transactions(vec![removed_hash]).len(), 1);
        assert_eq!(best_hashes(&pool)[0], honest_hash, "an impossible tip must not rank");
    }

    #[tokio::test]
    async fn sponsored_tip_validated_before_its_sender_is_drained_is_unbacked() {
        let (pool, client) = build_integration_pool();
        let [sender, payer] = std::array::from_fn(|_| signer());
        let (honest_hash, _) = setup_tip_race(&pool, &client, &signer()).await;
        fund(&client, payer.address());
        let tip = tipping_fees();
        client.add_account(sender.address(), ExtendedAccount::new(0, U256::from(2) * tip));
        let first = tipping_channel_8130(&sender, Some(&payer), 0, tip);
        pool.add_transaction(TransactionOrigin::External, first).await.unwrap();

        let second = tipping_channel_8130(&sender, Some(&payer), 1, tip);
        insert_after_balance_drop(&pool, second, U256::ZERO).await;
        let best = best_hashes(&pool);
        assert_eq!(best[0], honest_hash, "neither of the drained sender's tips may rank");
        assert_eq!(best.len(), 3);
    }

    #[tokio::test]
    async fn sponsored_sidecar_coinbase_tip_from_an_unfunded_sender_is_unbacked() {
        let (pool, client) = build_integration_pool();
        let [unfunded, payer] = std::array::from_fn(|_| signer());
        let (honest_hash, _) = setup_tip_race(&pool, &client, &signer()).await;
        fund(&client, payer.address());

        let transaction = tipping_channel_8130(&unfunded, Some(&payer), 0, tipping_fees());
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        assert!(is_pending(&pool, hash), "the payer still funds the transaction");
        assert_eq!(best_hashes(&pool)[0], honest_hash, "the payer does not back the sender's tip");
    }

    #[test]
    fn sidecar_balance_updates_are_payer_keyed_without_a_pool_scan() {
        let (pool, _) = build_integration_pool();
        let signer = signer();
        let payer = Address::repeat_byte(0xA5);
        let transaction = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        transaction.set_watch_set(WatchSet::new().watch(InvalidationKey::Balance(payer)));
        transaction.set_limit_class(LimitClass {
            sender: signer.address(),
            payer,
            classification_generation: 0,
            sender_locked: false,
            payer_locked: true,
            payer_trusted: false,
            payer_allowlisted: false,
            payer_balance: U256::from(1_000),
            max_cost: U256::from(1_000),
        });
        let hash = *transaction.hash();
        let admission = IntegrationPool::admission_for(&transaction).unwrap();
        pool.sidecar.pool.write().insert_validated(valid_pool_transaction(transaction), 0).unwrap();
        pool.guard.write().try_admit(admission).unwrap();

        pool.update_accounts(vec![ChangedAccount {
            address: signer.address(),
            nonce: 0,
            balance: U256::ZERO,
        }]);
        assert!(
            pool.get(&hash).is_some(),
            "a sponsored sidecar transaction must not be scanned against sender balance"
        );

        pool.update_accounts(vec![ChangedAccount {
            address: payer,
            nonce: 0,
            balance: U256::from(999),
        }]);
        assert!(pool.get(&hash).is_none());
        assert!(!pool.guard.read().contains(&hash));
    }

    #[test]
    fn register_block_expiry_drops_the_replaced_entry() {
        let (pool, _) = build_integration_pool();
        let replaced = TxHash::repeat_byte(0x11);
        let new_hash = TxHash::repeat_byte(0x22);

        // Seed the index with the soon-to-be-replaced transaction.
        pool.register_block_expiry(replaced, Some(100), None, None);
        assert_eq!(pool.block_expiry.read().len(), 1);

        // A fee-bump replacement must evict the stale entry rather than
        // accumulate it, so the index does not grow per replacement.
        pool.register_block_expiry(new_hash, Some(200), None, Some(replaced));
        assert_eq!(pool.block_expiry.read().len(), 1);

        // Only the new hash remains, tracked at its own bound.
        let mut index = pool.block_expiry.write();
        assert!(index.drain_expired(150).is_empty());
        assert_eq!(index.drain_expired(200), vec![new_hash]);
    }

    #[test]
    fn register_block_expiry_drops_a_replaced_entry_without_a_new_bound() {
        let (pool, _) = build_integration_pool();
        let replaced = TxHash::repeat_byte(0x33);
        let new_hash = TxHash::repeat_byte(0x44);

        pool.register_block_expiry(replaced, Some(100), None, None);
        assert_eq!(pool.block_expiry.read().len(), 1);

        // An unbounded replacement still clears the replaced hash's stale entry.
        pool.register_block_expiry(new_hash, None, None, Some(replaced));
        assert!(pool.block_expiry.read().is_empty());
    }

    #[tokio::test]
    async fn published_flashblock_frees_unscanned_protocol_nonce_for_unbumped_transaction() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000)
            .with_validity_predicates(vec![
                ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(100),
                },
                ValidityPredicate::FlashblockIndex {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(2),
                },
            ]);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();
        assert_eq!(publish(&pool, 100, 1), 0);
        assert!(pool.get(&original_hash).is_some());
        assert!(
            pool.add_transaction(TransactionOrigin::Local, signed_1559(&signer, 0)).await.is_err(),
            "same-nonce unbumped transaction must conflict before the deadline"
        );
        // No selection iterator has examined the original transaction.
        assert_eq!(publish(&pool, 100, 2), 1);
        assert!(pool.get(&original_hash).is_none());
        assert!(!pool.guard.read().contains(&original_hash));
        let replacement = signed_1559(&signer, 0);
        let replacement_hash = *replacement.hash();
        pool.add_transaction(TransactionOrigin::Local, replacement).await.unwrap();
        assert!(pool.get(&replacement_hash).is_some());
        assert_eq!(publish(&pool, 100, 2), 0);
        assert!(pool.get(&replacement_hash).is_some());
    }

    #[tokio::test]
    async fn published_flashblock_expires_eip1559_validity_metadata() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let original = signed_1559(&signer, 0).with_validity_predicates(vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1),
            },
        ]);
        let hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();
        assert_eq!(publish(&pool, 100, 1), 1);
        assert!(pool.get(&hash).is_none());
        pool.add_transaction(TransactionOrigin::Local, signed_1559(&signer, 0)).await.unwrap();
        assert!(pool.get(&hash).is_some());
    }

    #[tokio::test]
    async fn received_publication_frees_same_nonce_slot_without_publishing() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let predicates = vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(101),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(2),
            },
        ];
        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000)
            .with_validity_predicates(predicates.clone());
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        // Same nonce and fees as the original; the gas limit only makes it a distinct transaction.
        let unbumped = || signed_8130(&signer, U256::ZERO, 0, 0, 1_000, 999_999);

        // A follower observes flashblock 1 of the deadline block: the deadline still holds, so an
        // unbumped same-nonce replacement is priced against the original.
        assert_eq!(pool.expire_published(101, 1), 0);
        let early = unbumped();
        let err = pool.add_transaction(TransactionOrigin::Local, early).await.unwrap_err();
        assert!(matches!(err.kind, PoolErrorKind::ReplacementUnderpriced), "got {err:?}");
        assert!(pool.get(&original_hash).is_some());

        // Observing the deadline flashblock evicts the original and frees the slot, and a stale
        // position received afterwards is ignored.
        assert_eq!(pool.expire_published(101, 2), 1);
        assert!(pool.get(&original_hash).is_none());
        assert_eq!(pool.expire_published(101, 0), 0);
        let replacement = unbumped();
        let replacement_hash = *replacement.hash();
        pool.add_transaction(TransactionOrigin::Local, replacement)
            .await
            .expect("unbumped replacement is admitted once the deadline flashblock is received");
        assert!(pool.get(&replacement_hash).is_some());

        let late = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_001)
            .with_validity_predicates(predicates);
        let error = pool.add_transaction(TransactionOrigin::Local, late).await.unwrap_err();
        assert!(error.to_string().contains("validity deadline expired"), "{error}");
    }

    #[tokio::test]
    async fn published_position_rejects_late_expired_admission_and_preserves_future_block() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let predicates = vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(101),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(2),
            },
        ];
        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000)
            .with_validity_predicates(predicates.clone());
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();
        assert_eq!(publish(&pool, 100, 2), 0);
        assert!(pool.get(&original_hash).is_some(), "index resets next block");
        assert_eq!(publish(&pool, 101, 2), 1);
        let late = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_001)
            .with_validity_predicates(predicates);
        let error = pool.add_transaction(TransactionOrigin::Local, late).await.unwrap_err();
        assert!(error.to_string().contains("validity deadline expired"), "{error}");
        assert!(pool.get(&original_hash).is_none());
    }

    #[tokio::test]
    async fn canonical_reorg_restarts_flashblock_admission_and_expiry() {
        for (new_tip, rebuilt_block) in [(99, 100), (100, 101)] {
            for nonce_key in [U256::ZERO, U256::from(1)] {
                let (pool, client) = build_integration_pool();
                let signer = signer();
                fund(&client, signer.address());
                assert_eq!(publish(&pool, 101, 2), 0);

                let block = SealedBlock::seal_slow(BaseBlock {
                    header: alloy_consensus::Header {
                        number: new_tip,
                        gas_limit: pool.block_info().block_gas_limit,
                        timestamp: INTEGRATION_POOL_NOW_SECS,
                        ..Default::default()
                    },
                    body: Default::default(),
                });
                pool.on_canonical_state_change(CanonicalStateUpdate {
                    new_tip: &block,
                    pending_block_base_fee: 0,
                    pending_block_blob_fee: None,
                    changed_accounts: Vec::new(),
                    mined_transactions: Vec::new(),
                    update_kind: PoolUpdateKind::Reorg,
                });
                let transaction = self_paid_eoa_8130(&signer, nonce_key, 0, 0, 1_000)
                    .with_validity_predicates(vec![
                        ValidityPredicate::BlockNumber {
                            op: ValidityOperator::LessThanOrEqual,
                            value: U256::from(rebuilt_block),
                        },
                        ValidityPredicate::FlashblockIndex {
                            op: ValidityOperator::LessThanOrEqual,
                            value: U256::from(1),
                        },
                    ]);
                let hash = *transaction.hash();
                pool.add_transaction(TransactionOrigin::Local, transaction)
                    .await
                    .expect("orphaned publication must not reject a valid rebuilt-block deadline");
                assert!(pool.get(&hash).is_some());

                assert_eq!(publish(&pool, rebuilt_block, 1), 1);
                assert!(pool.get(&hash).is_none());
                assert!(!pool.guard.read().contains(&hash));
            }
        }
    }

    #[tokio::test]
    async fn canonical_commit_preserves_publication_ahead_of_committed_head() {
        for nonce_key in [U256::ZERO, U256::from(1)] {
            let (pool, client) = build_integration_pool();
            let signer = signer();
            fund(&client, signer.address());
            assert_eq!(publish(&pool, 101, 2), 0);

            // Canonical maintenance can lag publication of the block being built.
            let block = SealedBlock::seal_slow(BaseBlock {
                header: alloy_consensus::Header {
                    number: 100,
                    gas_limit: pool.block_info().block_gas_limit,
                    timestamp: INTEGRATION_POOL_NOW_SECS,
                    ..Default::default()
                },
                body: Default::default(),
            });
            pool.on_canonical_state_change(CanonicalStateUpdate {
                new_tip: &block,
                pending_block_base_fee: 0,
                pending_block_blob_fee: None,
                changed_accounts: Vec::new(),
                mined_transactions: Vec::new(),
                update_kind: PoolUpdateKind::Commit,
            });
            let transaction = self_paid_eoa_8130(&signer, nonce_key, 0, 0, 1_000)
                .with_validity_predicates(vec![
                    ValidityPredicate::BlockNumber {
                        op: ValidityOperator::LessThanOrEqual,
                        value: U256::from(101),
                    },
                    ValidityPredicate::FlashblockIndex {
                        op: ValidityOperator::LessThanOrEqual,
                        value: U256::from(1),
                    },
                ]);
            let hash = *transaction.hash();
            let error =
                pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap_err();
            assert!(error.to_string().contains("validity deadline expired"), "{error}");
            assert!(pool.get(&hash).is_none());
        }
    }

    #[tokio::test]
    async fn published_flashblock_expires_sidecar_but_not_block_only_deadline() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let sidecar = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000)
            .with_validity_predicates(vec![
                ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(100),
                },
                ValidityPredicate::FlashblockIndex {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(1),
                },
            ]);
        let sidecar_hash = *sidecar.hash();
        pool.add_transaction(TransactionOrigin::Local, sidecar).await.unwrap();
        let block_only = signed_1559(&signer, 0).with_validity_predicates(vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            },
        ]);
        let block_only_hash = *block_only.hash();
        pool.add_transaction(TransactionOrigin::Local, block_only).await.unwrap();
        // A build that did not publish must not invalidate either transaction.
        assert!(pool.get(&sidecar_hash).is_some());
        let failed = pool.publish_and_expire(100, 1, || Err::<(), _>("publish failed"));
        assert_eq!(failed, Err("publish failed"));
        assert!(pool.get(&sidecar_hash).is_some());
        assert_eq!(publish(&pool, 100, 1), 1);
        assert!(pool.get(&sidecar_hash).is_none());
        let continuation = signed_channel_tx(&signer, U256::from(1), 0, 1_000);
        let continuation_hash = *continuation.hash();
        pool.add_transaction(TransactionOrigin::Local, continuation).await.unwrap();
        assert!(pool.get(&continuation_hash).is_some());
        assert!(pool.get(&block_only_hash).is_some());
        pool.expire_by_block(100);
        assert!(pool.get(&block_only_hash).is_none());
    }

    #[tokio::test]
    async fn sidecar_validity_transaction_registers_block_expiry() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        // A finite 2D-nonce-channel EIP-8130 sidecar carrying a block-number
        // upper-bound predicate. Such a transaction has no time-based expiry, so
        // block-expiry registration is the only thing that prevents it from
        // lingering in the pool forever once it can no longer be included.
        let transaction = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000)
            .with_validity_predicates(vec![crate::ValidityPredicate::BlockNumber {
                op: crate::ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            }]);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();

        // The sidecar admission path must record the tx's last valid block.
        assert_eq!(pool.block_expiry.read().len(), 1);
        assert!(pool.get(&hash).is_some());

        // Exercise canonical maintenance at the inclusive last-valid-block boundary.
        for (committed_block, remains_live) in [(99, true), (100, false)] {
            let block = SealedBlock::seal_slow(BaseBlock {
                header: alloy_consensus::Header { number: committed_block, ..Default::default() },
                body: Default::default(),
            });
            pool.on_canonical_state_change(CanonicalStateUpdate {
                new_tip: &block,
                pending_block_base_fee: 0,
                pending_block_blob_fee: None,
                changed_accounts: Vec::new(),
                mined_transactions: Vec::new(),
                update_kind: PoolUpdateKind::Commit,
            });
            assert_eq!(pool.get(&hash).is_some(), remains_live);
            assert_eq!(pool.guard.read().contains(&hash), remains_live);
        }
    }

    fn sidecar_channel_tx(channel: u64) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer(),
            U256::from(channel),
            0,
            1_000,
        )))
    }

    /// A consumer of the merged pending listener that stops draining for longer than the channels
    /// can buffer must still be woken by sidecar transactions once it catches up. The merged
    /// receiver stays open on the protocol side, so a dropped sidecar listener would go unnoticed.
    #[tokio::test]
    async fn merged_pending_listener_keeps_sidecar_wakeups_after_consumer_stalls() {
        let mut listeners = SidecarListeners::<BasePooledTransaction>::default();
        let (_protocol_tx, protocol_rx) = mpsc::channel::<TxHash>(16);
        let sidecar_rx = listeners.subscribe_pending(TransactionListenerKind::All);
        let mut merged = merge_receivers(protocol_rx, sidecar_rx);

        // The forwarder fills the merged channel, blocks on it, and then the sidecar channel fills.
        let stalled = sidecar_channel_tx(1);
        for _ in 0..3 * SIDE_CAR_EVENT_CHANNEL_SIZE {
            listeners.broadcast_pending(&stalled);
            tokio::task::yield_now().await;
        }
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(20), merged.recv()).await
        {
        }

        let next = sidecar_channel_tx(2);
        listeners.broadcast_pending(&next);
        let woken = tokio::time::timeout(Duration::from_secs(1), merged.recv()).await;
        assert_eq!(woken.ok().flatten(), Some(*next.hash()));
    }

    #[tokio::test]
    async fn new_and_all_event_listeners_survive_full_channels() {
        let mut listeners = SidecarListeners::<BasePooledTransaction>::default();
        let mut new_events = listeners.subscribe_new_transactions(TransactionListenerKind::All);
        let mut all_events = listeners.subscribe_all();

        let stalled = sidecar_channel_tx(1);
        for _ in 0..=SIDE_CAR_EVENT_CHANNEL_SIZE {
            listeners.broadcast_pending_transaction(&stalled);
        }
        for _ in 0..SIDE_CAR_EVENT_CHANNEL_SIZE {
            new_events.recv().await.unwrap();
            all_events.next().await.unwrap();
        }

        let next = sidecar_channel_tx(2);
        listeners.broadcast_pending_transaction(&next);
        assert_eq!(
            new_events.recv().await.map(|event| *event.transaction.hash()),
            Some(*next.hash())
        );
        assert!(
            matches!(all_events.next().await, Some(FullTransactionEvent::Pending(hash)) if hash == *next.hash())
        );
    }

    #[tokio::test]
    async fn listeners_with_dropped_receivers_are_removed() {
        let mut listeners = SidecarListeners::<BasePooledTransaction>::default();
        drop(listeners.subscribe_pending(TransactionListenerKind::All));
        drop(listeners.subscribe_pending(TransactionListenerKind::PropagateOnly));
        drop(listeners.subscribe_new_transactions(TransactionListenerKind::All));
        drop(listeners.subscribe_new_transactions(TransactionListenerKind::PropagateOnly));
        drop(listeners.subscribe_all());

        listeners.broadcast_pending_transaction(&sidecar_channel_tx(1));

        assert!(listeners.pending_all.is_empty());
        assert!(listeners.pending_propagate.is_empty());
        assert!(listeners.new_all.is_empty());
        assert!(listeners.new_propagate.is_empty());
        assert!(listeners.all_events.is_empty());
    }
    // ----- Validity pool isolation -----

    /// A block bound none of these tests reach, so the transaction rests until
    /// something else removes it.
    const RESTING_EXPIRY_BLOCK: u64 = 1_000;

    fn resting_predicates() -> Vec<ValidityPredicate> {
        vec![ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(RESTING_EXPIRY_BLOCK),
        }]
    }

    fn validity_1559(
        signer: &PrivateKeySigner,
        nonce: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_1559_with_fees(signer, nonce, max_fee_per_gas, 0)
            .with_validity_predicates(resting_predicates())
    }

    fn validity_8130(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        self_paid_eoa_8130(signer, nonce_key, nonce_sequence, 0, max_fee_per_gas)
            .with_validity_predicates(resting_predicates())
    }

    fn funded_signer(
        client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
    ) -> PrivateKeySigner {
        let signer = signer();
        fund(client, signer.address());
        signer
    }

    fn in_protocol_pool(pool: &IntegrationPool, hash: TxHash) -> bool {
        pool.protocol_pool.get(&hash).is_some()
    }

    fn in_sidecar_pool(pool: &IntegrationPool, hash: TxHash) -> bool {
        pool.sidecar.pool.read().contains(&hash)
    }

    fn in_validity_pool(pool: &IntegrationPool, hash: TxHash) -> bool {
        pool.validity.pool.read().contains(&hash)
    }

    fn is_queued(pool: &IntegrationPool, hash: TxHash) -> bool {
        pool.queued_transactions().iter().any(|transaction| *transaction.hash() == hash)
    }

    /// Asserts `hash` is held by exactly `store` and nothing else.
    fn assert_store(pool: &IntegrationPool, hash: TxHash, store: PoolStore) {
        assert_eq!(in_protocol_pool(pool, hash), store == PoolStore::Protocol, "protocol {hash}");
        assert_eq!(in_sidecar_pool(pool, hash), store == PoolStore::Sidecar, "sidecar {hash}");
        assert_eq!(in_validity_pool(pool, hash), store == PoolStore::Validity, "validity {hash}");
        assert!(pool.get(&hash).is_some(), "lookup {hash}");
    }

    fn assert_gone(pool: &IntegrationPool, hash: TxHash) {
        assert!(pool.get(&hash).is_none(), "{hash} is still pooled");
        assert!(!in_protocol_pool(pool, hash) && !in_sidecar_pool(pool, hash));
        assert!(!in_validity_pool(pool, hash));
        assert!(!pool.guard.read().contains(&hash));
    }

    async fn add(pool: &IntegrationPool, transaction: BasePooledTransaction) -> TxHash {
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        hash
    }

    async fn add_err(pool: &IntegrationPool, transaction: BasePooledTransaction) -> PoolErrorKind {
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap_err().kind
    }

    fn commit_block(
        pool: &IntegrationPool,
        number: u64,
        changed_accounts: Vec<ChangedAccount>,
        mined_transactions: Vec<TxHash>,
    ) {
        canonical_update(
            pool,
            number,
            changed_accounts,
            mined_transactions,
            PoolUpdateKind::Commit,
        );
    }

    fn canonical_update(
        pool: &IntegrationPool,
        number: u64,
        changed_accounts: Vec<ChangedAccount>,
        mined_transactions: Vec<TxHash>,
        update_kind: PoolUpdateKind,
    ) {
        let block = SealedBlock::seal_slow(BaseBlock {
            header: alloy_consensus::Header {
                number,
                gas_limit: pool.block_info().block_gas_limit,
                timestamp: INTEGRATION_POOL_NOW_SECS + number * 2,
                ..Default::default()
            },
            body: Default::default(),
        });
        pool.on_canonical_state_change(CanonicalStateUpdate {
            new_tip: &block,
            pending_block_base_fee: 0,
            pending_block_blob_fee: None,
            changed_accounts,
            mined_transactions,
            update_kind,
        });
    }

    fn account_at(address: Address, nonce: u64) -> ChangedAccount {
        ChangedAccount { address, nonce, balance: U256::from(1_000_000_000_000_000_000u64) }
    }

    #[tokio::test]
    async fn every_validity_transaction_kind_lands_in_the_validity_pool() {
        let (pool, client) = build_integration_pool();
        let signers: Vec<_> = (0..7).map(|_| funded_signer(&client)).collect();
        let nonce_free_expiry = INTEGRATION_POOL_NOW_MS + 5_000;

        let eip1559 = add(&pool, validity_1559(&signers[0], 0, 1_000)).await;
        let key0 = add(&pool, validity_8130(&signers[1], U256::ZERO, 0, 1_000)).await;
        let channel = add(&pool, validity_8130(&signers[2], U256::from(7), 0, 1_000)).await;
        let nonce_free = add(
            &pool,
            signed_nonce_free_tx(&signers[3], nonce_free_expiry, 1_000)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        let plain_1559 = add(&pool, signed_1559(&signers[4], 0)).await;
        let plain_channel =
            add(&pool, self_paid_eoa_8130(&signers[5], U256::from(7), 0, 0, 1_000)).await;
        let plain_nonce_free =
            add(&pool, signed_nonce_free_tx(&signers[6], nonce_free_expiry, 1_000)).await;

        for hash in [eip1559, key0, channel, nonce_free] {
            assert_store(&pool, hash, PoolStore::Validity);
            assert!(is_pending(&pool, hash), "{hash} should be pending");
        }
        assert_store(&pool, plain_1559, PoolStore::Protocol);
        assert_store(&pool, plain_channel, PoolStore::Sidecar);
        assert_store(&pool, plain_nonce_free, PoolStore::Sidecar);

        let expected_bytes: usize = [eip1559, key0, channel, nonce_free]
            .iter()
            .map(|hash| pool.get(hash).unwrap().transaction.size())
            .sum();
        assert_eq!(pool.validity_pool_occupancy(), (4, expected_bytes));
        let size = pool.pool_size();
        assert_eq!((size.pending, size.queued, size.total), (7, 0, 7));
        assert_eq!(pool.all_transactions().pending.len(), 7);
        assert_eq!(best_hashes(&pool).len(), 7);
    }

    #[tokio::test]
    async fn validity_transaction_extends_the_pending_standard_run() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        let standard1 = add(&pool, signed_1559(&signer, 1)).await;
        let validity2 = add(&pool, validity_1559(&signer, 2, 1_000)).await;
        let validity3 = add(&pool, validity_1559(&signer, 3, 1_000)).await;

        assert_store(&pool, validity2, PoolStore::Validity);
        assert!(is_pending(&pool, validity2) && is_pending(&pool, validity3));
        assert_eq!(best_hashes(&pool), vec![standard0, standard1, validity2, validity3]);
        assert_eq!(
            pool.get_highest_consecutive_transaction_by_sender(signer.address(), 0)
                .map(|transaction| *transaction.hash()),
            Some(validity3)
        );
        assert_eq!(
            pool.get_highest_transaction_by_sender(signer.address()).map(|tx| *tx.hash()),
            Some(validity3)
        );
        assert_eq!(
            pool.get_transaction_by_sender_and_nonce(signer.address(), 2).map(|tx| *tx.hash()),
            Some(validity2)
        );
        assert_eq!(
            pool.get_pending_transaction_by_sender_and_nonce(signer.address(), 3)
                .map(|tx| *tx.hash()),
            Some(validity3)
        );
        assert_eq!(pool.get_transactions_by_sender(signer.address()).len(), 4);
        assert_eq!(pool.get_pending_transactions_by_sender(signer.address()).len(), 4);
    }

    #[tokio::test]
    async fn validity_transaction_behind_a_gap_waits_for_the_standard_run() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity2 = validity_1559(&signer, 2, 1_000);
        let validity2_hash = *validity2.hash();
        let mut events = pool
            .add_transaction_and_subscribe(TransactionOrigin::External, validity2)
            .await
            .unwrap();
        assert!(matches!(events.next().await, Some(TransactionEvent::Queued)));
        assert!(is_queued(&pool, validity2_hash) && !is_pending(&pool, validity2_hash));
        assert!(best_hashes(&pool).is_empty());
        assert!(pool.get_highest_consecutive_transaction_by_sender(signer.address(), 0).is_none());

        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        assert!(is_queued(&pool, validity2_hash), "nonce 1 is still missing");
        assert_eq!(best_hashes(&pool), vec![standard0]);

        let standard1 = add(&pool, signed_1559(&signer, 1)).await;
        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
        assert!(is_pending(&pool, validity2_hash));
        assert_eq!(best_hashes(&pool), vec![standard0, standard1, validity2_hash]);
        assert_eq!(
            pool.get_highest_consecutive_transaction_by_sender(signer.address(), 0)
                .map(|tx| *tx.hash()),
            Some(validity2_hash)
        );
    }

    #[tokio::test]
    async fn validity_lane_fills_its_own_gap() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        assert!(is_queued(&pool, validity1));
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        assert!(is_pending(&pool, validity0) && is_pending(&pool, validity1));
        assert_eq!(best_hashes(&pool), vec![validity0, validity1]);
        let size = pool.pool_size();
        assert_eq!((size.pending, size.queued), (2, 0));
    }

    #[tokio::test]
    async fn standard_transaction_behind_a_resting_validity_transaction_stays_queued() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        let standard1 = add(&pool, signed_1559(&signer, 1)).await;

        assert_store(&pool, standard1, PoolStore::Protocol);
        assert!(is_queued(&pool, standard1), "the protocol pool sees a nonce gap");
        let size = pool.pool_size();
        assert_eq!((size.pending, size.queued), (1, 1));
        // The merge still releases it, right behind the validity run it waits for.
        assert_eq!(best_hashes(&pool), vec![validity0, standard1]);

        // Dropping the validity transaction leaves the standard one gapped.
        pool.remove_transactions(vec![validity0]);
        assert_gone(&pool, validity0);
        assert!(is_queued(&pool, standard1));

        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        assert_eq!(best_hashes(&pool), vec![standard0, standard1]);
    }

    #[tokio::test]
    async fn mining_the_standard_prefix_keeps_the_validity_successor_pending() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        let validity2 = add(&pool, validity_1559(&signer, 2, 1_000)).await;
        assert!(is_pending(&pool, validity1));

        commit_block(&pool, 1, vec![account_at(signer.address(), 1)], vec![standard0]);
        assert_gone(&pool, standard0);
        assert!(is_pending(&pool, validity1) && is_pending(&pool, validity2));
        assert_eq!(
            pool.validity.pool.read().lane_cursor(&(signer.address(), U256::ZERO)),
            Some(LaneCursor::at(1))
        );
        assert_eq!(best_hashes(&pool), vec![validity1, validity2]);

        // Mining a validity transaction prunes it and advances its own lane.
        commit_block(&pool, 2, vec![account_at(signer.address(), 2)], vec![validity1]);
        assert_gone(&pool, validity1);
        assert!(is_pending(&pool, validity2));
        assert_eq!(pool.validity_pool_occupancy().0, 1);

        // A canonical nonce past the lane drops what it left behind.
        commit_block(&pool, 3, vec![account_at(signer.address(), 3)], Vec::new());
        assert_gone(&pool, validity2);
        assert_eq!(pool.validity_pool_occupancy(), (0, 0));
    }

    #[tokio::test]
    async fn dropping_the_standard_prefix_queues_the_validity_successor() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        let validity1 = validity_1559(&signer, 1, 1_000);
        let validity1_hash = *validity1.hash();
        let mut events = pool
            .add_transaction_and_subscribe(TransactionOrigin::External, validity1)
            .await
            .unwrap();
        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));

        pool.remove_transactions(vec![standard0]);
        assert!(matches!(events.next().await, Some(TransactionEvent::Queued)));
        assert!(is_queued(&pool, validity1_hash) && !is_pending(&pool, validity1_hash));
        assert!(best_hashes(&pool).is_empty());

        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
        assert_eq!(best_hashes(&pool), vec![standard0, validity1_hash]);
    }

    #[tokio::test]
    async fn removing_standard_descendants_queues_but_keeps_validity_successors() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        let standard2 = add(&pool, signed_1559(&signer, 2)).await;
        assert!(is_queued(&pool, standard2), "reth sees a gap at nonce 1");
        assert_eq!(best_hashes(&pool), vec![standard0, validity1, standard2]);

        let removed: Vec<_> = pool
            .remove_transactions_and_descendants(vec![standard0])
            .iter()
            .map(|tx| *tx.hash())
            .collect();
        assert_eq!(removed, vec![standard0, standard2]);
        assert_store(&pool, validity1, PoolStore::Validity);
        assert!(is_queued(&pool, validity1));

        // Removing a validity transaction with descendants cascades within its lane.
        let validity2 = add(&pool, validity_1559(&signer, 2, 1_000)).await;
        let removed: Vec<_> = pool
            .remove_transactions_and_descendants(vec![validity1])
            .iter()
            .map(|tx| *tx.hash())
            .collect();
        assert_eq!(removed, vec![validity1, validity2]);
        assert_eq!(pool.validity_pool_occupancy(), (0, 0));
    }

    #[tokio::test]
    async fn balance_drop_that_parks_the_standard_run_queues_validity_successors() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        assert_eq!(best_hashes(&pool), vec![standard0, validity1]);

        set_balance(&pool, signer.address(), U256::ZERO);
        assert!(is_queued(&pool, standard0), "reth parks the insolvent standard transaction");
        assert!(is_queued(&pool, validity1), "the validity successor follows it");
        assert!(best_hashes(&pool).is_empty());

        set_balance(&pool, signer.address(), U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(best_hashes(&pool), vec![standard0, validity1]);
    }

    #[tokio::test]
    async fn protocol_nonce_slot_is_exclusive_across_stores_under_the_price_bump() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity = validity_1559(&signer, 0, 1_000);
        let validity_hash = *validity.hash();
        let mut validity_events = pool
            .add_transaction_and_subscribe(TransactionOrigin::External, validity)
            .await
            .unwrap();
        assert!(matches!(validity_events.next().await, Some(TransactionEvent::Pending)));

        // The unbumped standard copy is the same signed transaction, so it is a
        // duplicate rather than a replacement.
        let error = add_err(&pool, signed_1559_with_fees(&signer, 0, 1_000, 0)).await;
        assert!(matches!(error, PoolErrorKind::AlreadyImported), "{error:?}");
        for underpriced in [1_001, 1_099] {
            let error = add_err(&pool, signed_1559_with_fees(&signer, 0, underpriced, 0)).await;
            assert!(matches!(error, PoolErrorKind::ReplacementUnderpriced), "{error:?}");
            assert_store(&pool, validity_hash, PoolStore::Validity);
        }

        let standard = add(&pool, signed_1559_with_fees(&signer, 0, 1_100, 0)).await;
        assert_store(&pool, standard, PoolStore::Protocol);
        assert_gone(&pool, validity_hash);
        assert!(matches!(
            validity_events.next().await,
            Some(TransactionEvent::Replaced(replaced_by)) if replaced_by == standard
        ));
        assert_eq!(pool.block_expiry.read().len(), 0, "the replaced expiry entry is dropped");

        let error = add_err(&pool, validity_1559(&signer, 0, 1_200)).await;
        assert!(matches!(error, PoolErrorKind::ReplacementUnderpriced), "{error:?}");
        assert_store(&pool, standard, PoolStore::Protocol);

        let bumped = add(&pool, validity_1559(&signer, 0, 1_210)).await;
        assert_store(&pool, bumped, PoolStore::Validity);
        assert_gone(&pool, standard);
        assert_eq!(pool.block_expiry.read().len(), 1);
        assert_eq!(best_hashes(&pool), vec![bumped]);

        let error = add_err(&pool, validity_1559(&signer, 0, 1_210)).await;
        assert!(matches!(error, PoolErrorKind::AlreadyImported), "{error:?}");
        let error = add_err(&pool, signed_1559_with_fees(&signer, 0, 1_100, 0)).await;
        assert!(matches!(error, PoolErrorKind::ReplacementUnderpriced), "{error:?}");
        let size = pool.pool_size();
        assert_eq!((size.pending, size.total), (1, 1));
    }

    #[tokio::test]
    async fn cross_store_replacement_cuts_the_validity_lane_behind_the_newcomer() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        let validity2 = add(&pool, validity_1559(&signer, 2, 1_000)).await;
        assert_eq!(best_hashes(&pool), vec![standard0, validity1, validity2]);

        // A standard transaction replacing validity nonce 1 leaves nonce 2 pending
        // behind the new standard run.
        let standard1 = add(&pool, signed_1559_with_fees(&signer, 1, 1_100, 0)).await;
        assert_gone(&pool, validity1);
        assert!(is_pending(&pool, validity2));
        assert_eq!(best_hashes(&pool), vec![standard0, standard1, validity2]);

        // A validity transaction replacing standard nonce 0 parks standard nonce 1
        // in reth and, with it, validity nonce 2. The merge still bridges the
        // standard transaction behind the validity head; validity nonce 2 waits
        // for the next canonical update.
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_100)).await;
        assert_gone(&pool, standard0);
        assert!(is_queued(&pool, standard1));
        assert!(is_queued(&pool, validity2));
        assert_eq!(best_hashes(&pool), vec![validity0, standard1]);
    }

    #[tokio::test]
    async fn channel_validity_transactions_follow_the_sidecar_run() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let channel = U256::from(3);
        let lane = (signer.address(), channel);

        let sidecar0 = add(&pool, self_paid_eoa_8130(&signer, channel, 0, 0, 1_000)).await;
        let validity1 = add(&pool, validity_8130(&signer, channel, 1, 1_000)).await;
        assert_store(&pool, sidecar0, PoolStore::Sidecar);
        assert_store(&pool, validity1, PoolStore::Validity);
        assert!(is_pending(&pool, validity1));
        assert_eq!(
            pool.validity.pool.read().lane_cursor(&lane),
            Some(LaneCursor { state_nonce: 0, next_nonce: 1 })
        );

        // A sidecar successor behind the validity transaction is gapped.
        let sidecar2 = add(&pool, self_paid_eoa_8130(&signer, channel, 2, 0, 1_000)).await;
        assert!(is_queued(&pool, sidecar2));
        assert_eq!(best_hashes(&pool), vec![sidecar0, validity1, sidecar2]);

        // Same-slot admission across the sidecar boundary follows the price bump.
        let error = add_err(&pool, self_paid_eoa_8130(&signer, channel, 1, 0, 1_050)).await;
        assert!(matches!(error, PoolErrorKind::ReplacementUnderpriced), "{error:?}");
        let sidecar1 = add(&pool, self_paid_eoa_8130(&signer, channel, 1, 0, 1_100)).await;
        assert_gone(&pool, validity1);
        assert_eq!(best_hashes(&pool), vec![sidecar0, sidecar1, sidecar2]);
        assert!(pool.validity.pool.read().lane_cursor(&lane).is_none());

        // And back: a validity transaction takes the sidecar head.
        let validity0 = add(&pool, validity_8130(&signer, channel, 0, 1_100)).await;
        assert_gone(&pool, sidecar0);
        assert_store(&pool, validity0, PoolStore::Validity);
        assert!(is_queued(&pool, sidecar1) && is_queued(&pool, sidecar2));
        assert_eq!(best_hashes(&pool), vec![validity0, sidecar1, sidecar2]);
    }

    #[tokio::test]
    async fn a_cross_store_replacement_evicted_on_insert_keeps_the_incumbent_reserved() {
        let (pool, client) = build_integration_pool();
        let pool = pool.with_validity_pool_limits(ValidityPoolLimits::inherit().with_max_txs(1));
        let signer = funded_signer(&client);
        let channel = U256::from(3);
        let incumbent = add(&pool, self_paid_eoa_8130(&signer, channel, 0, 0, 1_000)).await;
        assert!(pool.guard.read().contains(&incumbent));
        let resting = add(&pool, validity_1559(&funded_signer(&client), 0, 5_000)).await;

        // The bumped validity copy outbids the sidecar incumbent but is the
        // cheapest member of the full validity pool, so it is evicted on insert.
        let newcomer = validity_8130(&signer, channel, 0, 1_100);
        let newcomer_hash = *newcomer.hash();
        let error = add_err(&pool, newcomer).await;
        assert!(matches!(error, PoolErrorKind::DiscardedOnInsert), "{error:?}");
        assert_store(&pool, incumbent, PoolStore::Sidecar);
        assert_store(&pool, resting, PoolStore::Validity);
        assert!(pool.guard.read().contains(&incumbent), "the incumbent keeps its reservation");
        assert!(!pool.guard.read().contains(&newcomer_hash));

        // A replacement that survives eviction takes over the reservation.
        let replacement = add(&pool, validity_8130(&signer, channel, 0, 6_000)).await;
        assert_store(&pool, replacement, PoolStore::Validity);
        assert_gone(&pool, incumbent);
        assert!(pool.guard.read().contains(&replacement));
        assert!(!pool.guard.read().contains(&incumbent));
    }

    #[tokio::test]
    async fn mined_sidecar_head_promotes_the_validity_successor_on_its_channel() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let channel = U256::from(3);
        let sidecar0 = add(&pool, self_paid_eoa_8130(&signer, channel, 0, 0, 1_000)).await;
        let validity1 = add(&pool, validity_8130(&signer, channel, 1, 1_000)).await;
        let validity2 = add(&pool, validity_8130(&signer, channel, 2, 1_000)).await;

        commit_block(&pool, 1, vec![account_at(signer.address(), 0)], vec![sidecar0]);
        assert_gone(&pool, sidecar0);
        assert!(is_pending(&pool, validity1) && is_pending(&pool, validity2));
        assert_eq!(
            pool.validity.pool.read().lane_cursor(&(signer.address(), channel)),
            Some(LaneCursor::at(1))
        );

        // A nonce-manager state diff advancing the channel past nonce 1 drops it.
        let slot = NonceManagerStorage::nonce_slot(signer.address(), channel).unwrap();
        let removed = pool.apply_state_diff(&[AccountStateDiff {
            address: NonceManagerStorage::ADDRESS,
            changed_slots: vec![(B256::from(slot), U256::from(2))],
            ..Default::default()
        }]);
        assert_eq!(removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![validity1]);
        assert!(is_pending(&pool, validity2));
        assert!(pool.guard.read().contains(&validity2));
    }

    #[tokio::test]
    async fn nonce_free_validity_transactions_are_isolated_and_expire() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let expiry = INTEGRATION_POOL_NOW_MS + 5_000;
        let validity = signed_nonce_free_tx(&signer, expiry, 1_000)
            .with_validity_predicates(resting_predicates());
        let validity_hash = *validity.hash();
        let mut events = pool
            .add_transaction_and_subscribe(TransactionOrigin::External, validity)
            .await
            .unwrap();
        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
        assert_store(&pool, validity_hash, PoolStore::Validity);
        assert!(pool.guard.read().contains(&validity_hash));
        assert_eq!(best_hashes(&pool), vec![validity_hash]);

        // Resubmission is rejected from the validity pool, not duplicated in the sidecar.
        let error = add_err(
            &pool,
            signed_nonce_free_tx(&signer, expiry, 1_000)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        assert!(matches!(error, PoolErrorKind::AlreadyImported), "{error:?}");

        // The same replay id without predicates is the sidecar's slot: it must
        // bump the validity pool's copy, which it then replaces.
        let error = add_err(&pool, signed_nonce_free_tx(&signer, expiry, 1_050)).await;
        assert!(matches!(error, PoolErrorKind::ReplacementUnderpriced), "{error:?}");
        let plain = add(&pool, signed_nonce_free_tx(&signer, expiry, 1_100)).await;
        assert!(matches!(
            events.next().await,
            Some(TransactionEvent::Replaced(replaced_by)) if replaced_by == plain
        ));
        assert_gone(&pool, validity_hash);
        assert_store(&pool, plain, PoolStore::Sidecar);

        // And back into the validity pool under the same rule.
        let bumped = signed_nonce_free_tx(&signer, expiry, 1_210)
            .with_validity_predicates(resting_predicates());
        let bumped_hash = *bumped.hash();
        let mut events =
            pool.add_transaction_and_subscribe(TransactionOrigin::External, bumped).await.unwrap();
        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
        assert_gone(&pool, plain);
        assert_store(&pool, bumped_hash, PoolStore::Validity);

        // Time passes beyond `valid_before`: canonical maintenance expires it.
        commit_block(&pool, 5, Vec::new(), Vec::new());
        assert!(matches!(events.next().await, Some(TransactionEvent::Discarded)));
        assert_gone(&pool, bumped_hash);
        assert_eq!(pool.validity_pool_occupancy(), (0, 0));
    }

    #[tokio::test]
    async fn a_standard_flood_cannot_evict_validity_transactions() {
        let config =
            PoolConfig { pending_limit: SubPoolLimit::new(2, usize::MAX), ..PoolConfig::default() };
        let (pool, client) = build_integration_pool_with_config(config);
        let mut validity = Vec::new();
        for _ in 0..2 {
            let signer = funded_signer(&client);
            validity.push(add(&pool, validity_1559(&signer, 0, 1_000)).await);
        }

        let mut standard = Vec::new();
        for _ in 0..6 {
            let signer = funded_signer(&client);
            let transaction = signed_1559_with_fees(&signer, 0, 10_000, 5_000);
            standard.push(*transaction.hash());
            // reth discards the worst standard transaction, which may be the newcomer.
            let _ = pool.add_transaction(TransactionOrigin::External, transaction).await;
        }

        for hash in &validity {
            assert_store(&pool, *hash, PoolStore::Validity);
            assert!(is_pending(&pool, *hash));
        }
        let surviving_standard =
            standard.iter().filter(|hash| in_protocol_pool(&pool, **hash)).count();
        assert!(surviving_standard <= 2, "reth enforces its own pending limit");
        assert_eq!(pool.validity_pool_occupancy().0, 2);
    }

    #[tokio::test]
    async fn a_validity_flood_cannot_evict_standard_transactions() {
        let config =
            PoolConfig { pending_limit: SubPoolLimit::new(2, usize::MAX), ..PoolConfig::default() };
        let (pool, client) = build_integration_pool_with_config(config);
        let mut standard = Vec::new();
        for _ in 0..2 {
            let signer = funded_signer(&client);
            standard.push(add(&pool, signed_1559(&signer, 0)).await);
        }

        let mut validity = Vec::new();
        for fee in [5_000, 6_000, 7_000, 8_000] {
            let signer = funded_signer(&client);
            validity.push(add(&pool, validity_1559(&signer, 0, fee)).await);
        }
        // A newcomer cheaper than everything resting is refused outright.
        let cheap_signer = funded_signer(&client);
        let error = add_err(&pool, validity_1559(&cheap_signer, 0, 1_000)).await;
        assert!(matches!(error, PoolErrorKind::DiscardedOnInsert), "{error:?}");

        for hash in &standard {
            assert_store(&pool, *hash, PoolStore::Protocol);
            assert!(is_pending(&pool, *hash));
        }
        assert_eq!(pool.validity_pool_occupancy().0, 2);
        assert!(!in_validity_pool(&pool, validity[0]) && !in_validity_pool(&pool, validity[1]));
        assert!(in_validity_pool(&pool, validity[2]) && in_validity_pool(&pool, validity[3]));
        assert_gone(&pool, validity[0]);
        let size = pool.pool_size();
        assert_eq!((size.pending, size.total), (4, 4));
    }

    #[tokio::test]
    async fn validity_pool_limits_override_the_inherited_pending_limit() {
        let (pool, client) = build_integration_pool();
        let pool = pool.with_validity_pool_limits(ValidityPoolLimits::inherit().with_max_txs(1));
        let incumbent_signer = funded_signer(&client);
        let incumbent = add(&pool, validity_1559(&incumbent_signer, 0, 2_000)).await;

        let cheap_signer = funded_signer(&client);
        let error = add_err(&pool, validity_1559(&cheap_signer, 0, 1_000)).await;
        assert!(matches!(error, PoolErrorKind::DiscardedOnInsert), "{error:?}");
        assert_store(&pool, incumbent, PoolStore::Validity);

        let rich_signer = funded_signer(&client);
        let rich = add(&pool, validity_1559(&rich_signer, 0, 3_000)).await;
        assert_store(&pool, rich, PoolStore::Validity);
        assert_gone(&pool, incumbent);
        assert_eq!(pool.validity_pool_occupancy().0, 1);

        // The protocol pool keeps its own, much larger, limit.
        for _ in 0..3 {
            let signer = funded_signer(&client);
            add(&pool, signed_1559(&signer, 0)).await;
        }
        assert_eq!(pool.pool_size().pending, 4);
    }

    #[tokio::test]
    async fn validity_pool_byte_limit_evicts_the_cheapest_transaction() {
        let (pool, client) = build_integration_pool();
        let probe = validity_1559(&funded_signer(&client), 0, 1_000);
        let one_transaction = probe.size();
        let pool = pool.with_validity_pool_limits(
            ValidityPoolLimits::inherit().with_max_size(one_transaction),
        );
        let cheap = add(&pool, probe).await;
        let rich = add(&pool, validity_1559(&funded_signer(&client), 0, 2_000)).await;
        assert_gone(&pool, cheap);
        assert_store(&pool, rich, PoolStore::Validity);
        assert_eq!(pool.validity_pool_occupancy(), (1, one_transaction));
    }

    #[tokio::test]
    async fn validity_pool_account_slots_cap_transactions_per_sender() {
        let (pool, client) = build_integration_pool();
        let pool =
            pool.with_validity_pool_limits(ValidityPoolLimits::inherit().with_max_account_slots(2));
        let signer = funded_signer(&client);
        add(&pool, validity_1559(&signer, 1, 1_000)).await;
        add(&pool, validity_1559(&signer, 2, 1_000)).await;
        let error = add_err(&pool, validity_1559(&signer, 3, 1_000)).await;
        assert!(
            matches!(error, PoolErrorKind::SpammerExceededCapacity(address) if address == signer.address()),
            "{error:?}"
        );
        // Filling the lane head is exempt, as it is in reth: it unblocks what is held.
        let head = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        assert!(is_pending(&pool, head));
        assert_eq!(pool.validity_pool_occupancy().0, 3);
        assert_eq!(pool.pool_size().pending, 3);
    }

    #[tokio::test]
    async fn block_expiry_frees_the_validity_pool_slot() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity = signed_1559(&signer, 0).with_validity_predicates(vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(5),
            },
        ]);
        let validity_hash = *validity.hash();
        let mut events = pool
            .add_transaction_and_subscribe(TransactionOrigin::External, validity)
            .await
            .unwrap();
        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
        assert_store(&pool, validity_hash, PoolStore::Validity);
        assert_eq!(pool.block_expiry.read().len(), 1);

        commit_block(&pool, 4, Vec::new(), Vec::new());
        assert_store(&pool, validity_hash, PoolStore::Validity);
        commit_block(&pool, 5, Vec::new(), Vec::new());
        assert!(matches!(events.next().await, Some(TransactionEvent::Discarded)));
        assert_gone(&pool, validity_hash);
        assert_eq!(pool.block_expiry.read().len(), 0);
        assert_eq!(pool.validity_pool_occupancy(), (0, 0));

        // The freed slot admits an unbumped standard transaction.
        let standard = add(&pool, signed_1559(&signer, 0)).await;
        assert_store(&pool, standard, PoolStore::Protocol);
    }

    #[tokio::test]
    async fn published_flashblock_expires_validity_pool_members() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity = signed_1559(&signer, 0).with_validity_predicates(vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(100),
            },
            ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1),
            },
        ]);
        let validity_hash = add(&pool, validity).await;
        let standard1 = add(&pool, signed_1559(&signer, 1)).await;
        assert_store(&pool, validity_hash, PoolStore::Validity);
        assert!(is_queued(&pool, standard1));

        assert_eq!(publish(&pool, 100, 1), 1);
        assert_gone(&pool, validity_hash);
        assert!(is_queued(&pool, standard1), "the standard successor still waits for nonce 0");
        let standard0 = add(&pool, signed_1559(&signer, 0)).await;
        assert_eq!(best_hashes(&pool), vec![standard0, standard1]);
    }

    #[tokio::test]
    async fn account_nonce_state_diff_advances_the_validity_lane() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;

        let removed = pool.apply_state_diff(&[AccountStateDiff {
            address: signer.address(),
            nonce: Some(1),
            ..Default::default()
        }]);
        assert_eq!(removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![validity0]);
        assert_gone(&pool, validity0);
        assert!(is_pending(&pool, validity1));
        assert_eq!(
            pool.validity.pool.read().lane_cursor(&(signer.address(), U256::ZERO)),
            Some(LaneCursor::at(1))
        );
    }

    #[tokio::test]
    async fn update_accounts_re_anchors_validity_lanes_to_the_new_nonce() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        let standard2 = add(&pool, signed_1559(&signer, 2)).await;
        assert!(is_queued(&pool, standard2));

        pool.update_accounts(vec![account_at(signer.address(), 1)]);
        assert_gone(&pool, validity0);
        assert!(is_pending(&pool, validity1));
        assert!(is_queued(&pool, standard2));

        pool.update_accounts(vec![account_at(signer.address(), 2)]);
        assert_gone(&pool, validity1);
        assert_eq!(best_hashes(&pool), vec![standard2]);
        assert_eq!(pool.validity_pool_occupancy(), (0, 0));
    }

    #[tokio::test]
    async fn remove_transactions_by_sender_clears_every_store() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let other = funded_signer(&client);
        let standard = add(&pool, signed_1559(&signer, 0)).await;
        let validity = add(&pool, validity_1559(&signer, 1, 1_000)).await;
        let channel_validity = add(&pool, validity_8130(&signer, U256::from(9), 0, 1_000)).await;
        let sidecar = add(&pool, self_paid_eoa_8130(&signer, U256::from(8), 0, 0, 1_000)).await;
        let others_validity = add(&pool, validity_1559(&other, 0, 1_000)).await;

        let removed: std::collections::HashSet<_> = pool
            .remove_transactions_by_sender(signer.address())
            .iter()
            .map(|tx| *tx.hash())
            .collect();
        assert_eq!(
            removed,
            std::collections::HashSet::from([standard, validity, channel_validity, sidecar])
        );
        for hash in [standard, validity, channel_validity, sidecar] {
            assert_gone(&pool, hash);
        }
        assert_store(&pool, others_validity, PoolStore::Validity);
        assert_eq!(pool.validity_pool_occupancy().0, 1);
    }

    #[tokio::test]
    async fn best_transactions_merge_ranks_across_stores_and_parks_invalid_lanes() {
        let (pool, client) = build_integration_pool();
        let standard_signer = funded_signer(&client);
        let validity_signer = funded_signer(&client);
        let cheap_standard = add(&pool, signed_1559_with_fees(&standard_signer, 0, 1_000, 1)).await;
        let rich_validity = add(&pool, validity_1559(&validity_signer, 0, 1_000)).await;
        let validity_successor = add(&pool, validity_1559(&validity_signer, 1, 1_000)).await;
        // `validity_1559` carries no priority fee, so the ordering ranks it below
        // a tipping standard transaction only when the tip is higher.
        let tipping_standard =
            add(&pool, signed_1559_with_fees(&standard_signer, 1, 1_000, 500)).await;

        let order = best_hashes(&pool);
        assert_eq!(order.len(), 4);
        assert!(
            order.iter().position(|hash| *hash == cheap_standard)
                < order.iter().position(|hash| *hash == tipping_standard),
            "nonce order within a sender is preserved: {order:?}"
        );
        assert!(
            order.iter().position(|hash| *hash == rich_validity)
                < order.iter().position(|hash| *hash == validity_successor)
        );

        // Marking a validity transaction invalid parks the rest of its lane.
        let mut best = pool.best_transactions();
        let mut yielded = Vec::new();
        while let Some(transaction) = best.next() {
            if *transaction.hash() == rich_validity {
                best.mark_invalid(
                    &transaction,
                    InvalidPoolTransactionError::Consensus(
                        reth_primitives_traits::transaction::error::InvalidTransactionError::TxTypeNotSupported,
                    ),
                );
            }
            yielded.push(*transaction.hash());
        }
        assert!(yielded.contains(&rich_validity));
        assert!(!yielded.contains(&validity_successor));
        assert!(yielded.contains(&cheap_standard) && yielded.contains(&tipping_standard));
    }

    /// The reviewer's case: a validity successor that outbids the standard
    /// transaction in front of it must still come out behind it.
    #[tokio::test]
    async fn validity_successor_outbidding_the_standard_run_waits_for_it() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let other = funded_signer(&client);
        let standard0 = add(&pool, signed_1559_with_fees(&signer, 0, 1_000, 1)).await;
        let rich_validity1 = add(
            &pool,
            signed_1559_with_fees(&signer, 1, 1_000, 900)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        let rich_validity2 = add(
            &pool,
            signed_1559_with_fees(&signer, 2, 1_000, 800)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        let unrelated = add(&pool, signed_1559_with_fees(&other, 0, 1_000, 500)).await;
        assert!(is_pending(&pool, rich_validity1) && is_pending(&pool, rich_validity2));

        // Priority alone would rank the validity transactions first.
        assert_eq!(
            best_hashes(&pool),
            vec![unrelated, standard0, rich_validity1, rich_validity2],
            "the standard predecessor is released before its validity successors"
        );

        // Once the builder drops the standard predecessor, its successors stay parked.
        let mut best = pool.best_transactions();
        let mut yielded = Vec::new();
        while let Some(transaction) = best.next() {
            if *transaction.hash() == standard0 {
                best.mark_invalid(&transaction, InvalidPoolTransactionError::Underpriced);
            }
            yielded.push(*transaction.hash());
        }
        assert_eq!(yielded, vec![unrelated, standard0]);

        // Lanes anchored at canonical state need no predecessor and keep their own order.
        pool.remove_transactions(vec![standard0]);
        let validity0 = add(
            &pool,
            signed_1559_with_fees(&signer, 0, 1_000, 1)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        assert_eq!(best_hashes(&pool), vec![unrelated, validity0, rich_validity1, rich_validity2]);
    }

    /// The builder's case: a predicate-delayed validity transaction must not
    /// keep the user's standard successors out of the block once it executes.
    #[tokio::test]
    async fn standard_descendants_behind_a_validity_run_are_released_after_it() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let other = funded_signer(&client);
        let validity0 = add(
            &pool,
            signed_1559_with_fees(&signer, 0, 1_000, 100)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        let standard1 = add(&pool, signed_1559_with_fees(&signer, 1, 1_000, 900)).await;
        let standard2 = add(&pool, signed_1559_with_fees(&signer, 2, 1_000, 950)).await;
        // Nonce 3 is missing: nonce 4 is not part of the bridged run.
        let standard4 = add(&pool, signed_1559_with_fees(&signer, 4, 1_000, 999)).await;
        let unrelated = add(&pool, signed_1559_with_fees(&other, 0, 1_000, 500)).await;
        assert!(is_queued(&pool, standard1) && is_queued(&pool, standard2));

        assert_eq!(best_hashes(&pool), vec![unrelated, validity0, standard1, standard2]);

        // A dropped validity head takes its standard descendants with it.
        let mut best = pool.best_transactions();
        let mut yielded = Vec::new();
        while let Some(transaction) = best.next() {
            if *transaction.hash() == validity0 {
                best.mark_invalid(&transaction, InvalidPoolTransactionError::Underpriced);
            }
            yielded.push(*transaction.hash());
        }
        assert_eq!(yielded, vec![unrelated, validity0]);

        // A validity run that is itself gapped releases nothing behind it.
        pool.remove_transactions(vec![validity0]);
        let validity3 = add(&pool, validity_1559(&signer, 3, 1_000)).await;
        assert!(is_queued(&pool, validity3) && is_queued(&pool, standard4));
        assert_eq!(best_hashes(&pool), vec![unrelated]);
    }

    /// The reviewer's second case: a waiting validity head must not starve
    /// the validity transactions of every other sender.
    #[tokio::test]
    async fn a_waiting_validity_head_does_not_starve_other_validity_senders() {
        let (pool, client) = build_integration_pool();
        let waiting = funded_signer(&client);
        let others: Vec<_> = (0..3).map(|_| funded_signer(&client)).collect();
        let standard0 = add(&pool, signed_1559_with_fees(&waiting, 0, 1_000, 1)).await;
        let waiting_validity = add(
            &pool,
            signed_1559_with_fees(&waiting, 1, 1_000, 900)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        let mut other_validity = Vec::new();
        for (index, other) in others.iter().enumerate() {
            other_validity.push(
                add(
                    &pool,
                    signed_1559_with_fees(other, 0, 1_000, 500 - index as u128)
                        .with_validity_predicates(resting_predicates()),
                )
                .await,
            );
        }

        let mut expected = other_validity.clone();
        expected.extend([standard0, waiting_validity]);
        assert_eq!(best_hashes(&pool), expected);

        // A builder that stops before the low-tip predecessor still gets every other lane.
        let first_three: Vec<_> =
            pool.best_transactions().take(3).map(|transaction| *transaction.hash()).collect();
        assert_eq!(first_three, other_validity);
    }

    /// The standard descendants may be in their store before the validity
    /// transaction arrives, and the validity lane may come and go.
    #[tokio::test]
    async fn standard_descendants_admitted_before_the_validity_run_are_bridged_too() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let standard1 = add(&pool, signed_1559(&signer, 1)).await;
        let standard2 = add(&pool, signed_1559(&signer, 2)).await;
        assert!(best_hashes(&pool).is_empty());

        let validity0 = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        assert_eq!(best_hashes(&pool), vec![validity0, standard1, standard2]);

        // A fresh validity lane meets the waiting descendants again.
        pool.remove_transactions(vec![validity0]);
        assert!(best_hashes(&pool).is_empty());
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_100)).await;
        assert_eq!(best_hashes(&pool), vec![validity0, standard1, standard2]);

        // The shared flag follows the standard store's hold on the lane: it is
        // dropped once nothing standard is left and set again by the next admission.
        let lane = (signer.address(), U256::ZERO);
        assert!(pool.validity.pool.read().is_shared_lane(&lane));
        pool.remove_transactions(vec![standard1, standard2]);
        assert!(!pool.validity.pool.read().is_shared_lane(&lane));
        assert_eq!(best_hashes(&pool), vec![validity0]);
        let standard1 = add(&pool, signed_1559(&signer, 1)).await;
        let standard2 = add(&pool, signed_1559(&signer, 2)).await;
        assert!(pool.validity.pool.read().is_shared_lane(&lane));
        assert_eq!(best_hashes(&pool), vec![validity0, standard1, standard2]);

        // Mining the validity transaction promotes the descendants in reth itself.
        commit_block(&pool, 1, vec![account_at(signer.address(), 1)], vec![validity0]);
        assert_gone(&pool, validity0);
        assert!(is_pending(&pool, standard1) && is_pending(&pool, standard2));
        assert_eq!(best_hashes(&pool), vec![standard1, standard2]);
    }

    #[tokio::test]
    async fn channel_validity_successor_waits_for_the_sidecar_head() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let channel = U256::from(4);
        let sidecar0 = add(&pool, self_paid_eoa_8130(&signer, channel, 0, 0, 1_000)).await;
        let rich_validity1 = add(
            &pool,
            signed_8130(&signer, channel, 1, 0, 1_000, 1_000_000)
                .with_validity_predicates(resting_predicates()),
        )
        .await;
        // Give the validity successor the higher priority through a tip.
        let rich_validity1 = {
            pool.remove_transactions(vec![rich_validity1]);
            let tipped = TxEip8130 {
                chain_id: test_chain_id(),
                sender: None,
                nonce_key: channel,
                nonce_sequence: 1,
                valid_after: 0,
                valid_before: 0,
                max_priority_fee_per_gas: 900,
                max_fee_per_gas: 1_000,
                gas_limit: 1_000_000,
                account_changes: Vec::new(),
                calls: Vec::new(),
                metadata: Bytes::new(),
                payer: None,
            };
            add(
                &pool,
                sign_eoa_8130(&signer, tipped).with_validity_predicates(resting_predicates()),
            )
            .await
        };
        assert!(is_pending(&pool, rich_validity1));
        assert_eq!(best_hashes(&pool), vec![sidecar0, rich_validity1]);
    }

    #[tokio::test]
    async fn pending_listener_announces_validity_transactions() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let mut pending = pool.pending_transactions_listener();
        let mut new_transactions = pool.new_transactions_listener();
        let validity = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        assert_eq!(pending.recv().await, Some(validity));
        let event = new_transactions.recv().await.unwrap();
        assert_eq!(*event.transaction.hash(), validity);
        assert!(matches!(event.subpool, SubPool::Pending));
    }

    #[tokio::test]
    async fn reorg_mined_validity_transaction_is_pruned_and_lane_follows() {
        let (pool, client) = build_integration_pool();
        let signer = funded_signer(&client);
        let validity0 = add(&pool, validity_1559(&signer, 0, 1_000)).await;
        let validity1 = add(&pool, validity_1559(&signer, 1, 1_000)).await;

        canonical_update(
            &pool,
            1,
            vec![account_at(signer.address(), 1)],
            vec![validity0],
            PoolUpdateKind::Reorg,
        );
        assert_gone(&pool, validity0);
        assert!(is_pending(&pool, validity1));
        assert!(pool.published_flashblock.read().is_none());
    }
}
