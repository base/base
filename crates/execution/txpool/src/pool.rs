//! Base transaction-pool wrapper that combines the protocol pool with a 2D nonce sidecar.

use std::{collections::HashMap, fmt, sync::Arc};

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B128, B256, TxHash, U256, map::AddressSet};
use base_common_precompiles::NonceManagerStorage;
use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStore, BlobStoreError, BlockInfo, FullTransactionEvent,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool, PoolResult, PoolSize,
    PoolTransaction, PoolUpdateKind, PropagatedTransactions, SubPool, TransactionEvents,
    TransactionListenerKind, TransactionOrigin, TransactionPool, TransactionPoolExt,
    TransactionValidationOutcome, TransactionValidationTaskExecutor, TransactionValidator,
    ValidPoolTransaction,
    pool::{AddedTransactionState, QueuedReason, TransactionEvent},
};
use tokio::{spawn, sync::mpsc};
use tracing::debug;

use crate::{
    Admission, BasePooledTx, BaseTransactionValidator, FlashblockExpiry, GuardLimits, GuardMetrics,
    InvalidationCause, InvalidationKey, LimitRejection, MempoolGuard, ParkableBestTransactions,
    ParkableTransactionPool, ParkedBestTransactions, PredicateContext, StateDiffInvalidation,
    ValidityPoolMetrics, ValidityPredicate,
    best::MergeBestTransactions,
    two_d_nonce_pool::{InsertOutcome, LaneId, LaneUpdate, TwoDNoncePool},
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

/// Wrapper around reth's transaction pool that adds a 2D nonce sidecar for EIP-8130 channels.
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
    nonce_pool: Arc<RwLock<TwoDNoncePool<T>>>,
    listeners: Arc<RwLock<SidecarListeners<T>>>,
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
            nonce_pool: Arc::clone(&self.nonce_pool),
            listeners: Arc::clone(&self.listeners),
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
    /// Creates a new wrapper around the reth protocol pool.
    pub fn new(
        protocol_pool: Pool<
            TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>,
            O,
            S,
        >,
        ordering: O,
    ) -> Self {
        let price_bump_config = protocol_pool.config().price_bumps;
        Self {
            protocol_pool,
            ordering,
            nonce_pool: Arc::new(RwLock::new(TwoDNoncePool::new(price_bump_config))),
            listeners: Arc::new(RwLock::new(SidecarListeners::default())),
            guard: Arc::new(RwLock::new(MempoolGuard::unlimited())),
            block_expiry: Arc::new(RwLock::new(crate::BlockExpiryIndex::new())),
            published_flashblock: Arc::new(RwLock::new(None)),
            protocol_admission_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Configures EIP-8130 admission limits. Call before sharing the pool.
    #[must_use]
    pub fn with_guard_limits(self, limits: GuardLimits) -> Self {
        *self.guard.write() = MempoolGuard::new(limits);
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

    fn is_sidecar_transaction(&self, transaction: &T) -> bool {
        transaction.is_eip8130_sidecar_transaction()
    }

    fn limit_rejection_error(
        hash: TxHash,
        rejection: LimitRejection,
    ) -> reth_transaction_pool::error::PoolError {
        GuardMetrics::admission_rejected(GuardMetrics::rejection_reason(rejection)).increment(1);
        let reason = match rejection {
            LimitRejection::SenderLimit => "sender EIP-8130 signature limit reached",
            LimitRejection::PayerLimit => "payer EIP-8130 signature limit reached",
            LimitRejection::PaymentLimit => "payer EIP-8130 payment limit reached",
            LimitRejection::PayerBalance => "payer cannot fund another EIP-8130 transaction",
        };
        debug!(reason = GuardMetrics::rejection_reason(rejection), "EIP-8130 admission rejected");
        reth_transaction_pool::error::PoolError::other(hash, reason)
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
        let (behind, lane_moves) = self.resolve_nonce_advances(&advances);
        // Move lane cursors before releasing what fell behind them, so a
        // successor that stays pending is not re-announced as promoted.
        let mut removed = Vec::new();
        if !lane_moves.is_empty() {
            let mut nonce_pool = self.nonce_pool.write();
            let mut listeners = self.listeners.write();
            let mut update = LaneUpdate::default();
            for (lane_id, nonce) in lane_moves {
                update.extend(nonce_pool.advance_lane_nonce(lane_id, nonce));
            }
            listeners.on_lane_update(&update);
            let mut guard = self.guard.write();
            for transaction in &update.discarded {
                guard.release(transaction.hash());
            }
            removed.extend(update.discarded);
        }
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
        removed.extend(self.remove_dropped_across_pools(dropped));
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
        let removed = self.remove_dropped_across_pools(dropped);
        GuardMetrics::record_bulk_invalidations(removed.len(), cause);
        removed
    }

    /// Resolves the watchers of each nonce advance. A watcher survives only when
    /// it is an EIP-8130 transaction sequenced on that nonce at or after the new
    /// value; every other watcher is returned to be invalidated, as an exact
    /// match would. Each surviving sidecar lane is returned with the nonce its
    /// cursor moves to.
    fn resolve_nonce_advances(
        &self,
        advances: &[(InvalidationKey, u64)],
    ) -> (Vec<TxHash>, Vec<(LaneId, u64)>) {
        let mut behind = Vec::new();
        let mut lane_moves = Vec::new();
        if advances.is_empty() {
            return (behind, lane_moves);
        }
        let watchers: Vec<_> = {
            let guard = self.guard.read();
            advances.iter().map(|(key, nonce)| (key, *nonce, guard.watchers(key))).collect()
        };
        let nonce_pool = self.nonce_pool.read();
        for (key, nonce, hashes) in watchers {
            let mut moved_lane = None;
            for hash in hashes {
                let transaction = nonce_pool.get(&hash).or_else(|| self.protocol_pool.get(&hash));
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
                match lane {
                    Some(lane) if nonce_pool.contains(&hash) => moved_lane = Some(lane),
                    Some(_) => {}
                    None => behind.push(hash),
                }
            }
            if let Some(lane) = moved_lane {
                lane_moves.push((lane, nonce));
            }
        }
        (behind, lane_moves)
    }

    fn remove_dropped_across_pools(
        &self,
        dropped: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        if dropped.is_empty() {
            return Vec::new();
        }
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(dropped);
        let mut removed = if protocol_hashes.is_empty() {
            Vec::new()
        } else {
            self.protocol_pool.remove_transactions(protocol_hashes)
        };
        if !sidecar_hashes.is_empty() {
            let sidecar_removed = self.nonce_pool.write().remove_transactions(&sidecar_hashes);
            if !sidecar_removed.is_empty() {
                self.listeners.write().on_discarded(&sidecar_removed);
            }
            removed.extend(sidecar_removed);
        }
        removed
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
        let removed = self.remove_dropped_across_pools(dropped);
        GuardMetrics::record_expiry_invalidations(removed.len());
        if !removed.is_empty() {
            debug!(count = removed.len(), "EIP-8130 transactions invalidated by expiry");
        }
    }

    /// Returns the block-expiry bound for a validated transaction's validity
    /// predicates, or `None` when they impose no finite block bound.
    fn validity_block_expiry_bound(validated: &TransactionValidationOutcome<T>) -> Option<u64> {
        let transaction = validated.as_valid_transaction()?;
        crate::ValidityPredicate::block_expiry_bound(
            transaction.transaction().validity_predicates(),
        )
    }

    /// Optional flashblock upper bound, paired with the block bound for pool eviction.
    fn validity_flashblock_expiry_bound(
        validated: &TransactionValidationOutcome<T>,
    ) -> Option<u64> {
        let transaction = validated.as_valid_transaction()?;
        ValidityPredicate::flashblock_expiry_bound(transaction.transaction().validity_predicates())
    }

    /// Rejects a late arrival whose deadline is behind the published position.
    /// Called while holding `protocol_admission_lock`, including on the sidecar path.
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
            if ValidityPredicate::is_batch_expired(
                transaction.transaction().validity_predicates(),
                &next,
            ) {
                return Err(reth_transaction_pool::error::PoolError::other(
                    hash,
                    "validity deadline expired at published flashblock",
                ));
            }
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
        let removed = self.remove_dropped_across_pools(expired);
        // Release any guard slots directly rather than deferring to the
        // reconciliation sweep: an EIP-8130 transaction may carry block_number
        // validity predicates, so an evicted tx can also hold guard capacity.
        // `release` is a no-op for the common EIP-1559 case that is not tracked.
        self.release_from_guard(&removed);
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
        let removed = self.remove_dropped_across_pools(expired);
        self.release_from_guard(&removed);
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

    /// Returns whether a validated transaction carries validity predicates, for
    /// lane-churn accounting. Invalid outcomes carry no pooled transaction and
    /// are never counted.
    fn has_validity_predicates(validated: &TransactionValidationOutcome<T>) -> bool {
        validated
            .as_valid_transaction()
            .is_some_and(|transaction| !transaction.transaction().validity_predicates().is_empty())
    }

    fn reconcile_guard(&self) {
        let tracked = self.guard.read().tracked_hashes();
        if tracked.is_empty() {
            return;
        }
        let nonce_pool = self.nonce_pool.read();
        let stale: Vec<_> = tracked
            .into_iter()
            .filter(|hash| self.protocol_pool.get(hash).is_none() && !nonce_pool.contains(hash))
            .collect();
        drop(nonce_pool);
        if stale.is_empty() {
            return;
        }

        // Recheck only stale candidates under admission serialization. The
        // initial full scan stays outside this critical section, while a
        // reserved-before-insert transaction cannot be mistaken for an orphan.
        let _admission_guard = self.protocol_admission_lock.lock();
        let nonce_pool = self.nonce_pool.read();
        let stale: Vec<_> = stale
            .into_iter()
            .filter(|hash| self.protocol_pool.get(hash).is_none() && !nonce_pool.contains(hash))
            .collect();
        drop(nonce_pool);
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

    fn protocol_replacement_hash(&self, sender: Address, nonce: u64) -> Option<TxHash> {
        self.protocol_pool
            .get_transactions_by_sender(sender)
            .into_iter()
            .find(|existing| existing.nonce() == nonce)
            .map(|existing| *existing.hash())
    }

    fn stale_classification_error(hash: TxHash) -> reth_transaction_pool::error::PoolError {
        reth_transaction_pool::error::PoolError::other(
            hash,
            "EIP-8130 admission classification changed during validation",
        )
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
        let _admission_guard = self.protocol_admission_lock.lock();
        // Reject stale validation before reth can replace the currently pooled transaction.
        self.ensure_protocol_classification_current(hash, &validated)?;
        self.ensure_not_published_expired(hash, &validated)?;
        let replaced = self.protocol_replacement_hash(sender, nonce);
        // Reserve guard capacity before reth publishes insertion events. A
        // tracked replacement bypasses this check and is reaccounted after the
        // protocol pool atomically accepts the fee bump.
        let pre_admitted = self.pre_admit_protocol_transaction(hash, replaced, &validated)?;
        // Capture the block-expiry bound and validity-lane flag before
        // `validated` is consumed below.
        let block_expiry_bound = Self::validity_block_expiry_bound(&validated);
        let flashblock_expiry_bound = Self::validity_flashblock_expiry_bound(&validated);
        let is_validity = Self::has_validity_predicates(&validated);
        let mut outcomes =
            self.protocol_pool.inner().add_transactions(origin, std::iter::once(validated));
        let outcome = match outcomes.pop() {
            Some(Ok(outcome)) => outcome,
            Some(Err(error)) => {
                if pre_admitted {
                    self.guard.write().release(&hash);
                }
                return Err(error);
            }
            None => {
                if pre_admitted {
                    self.guard.write().release(&hash);
                }
                return Err(reth_transaction_pool::error::PoolError::other(
                    hash,
                    "inner pool returned no outcome",
                ));
            }
        };
        self.gate_protocol_admission(hash, replaced, pre_admitted)?;
        self.register_block_expiry(hash, block_expiry_bound, flashblock_expiry_bound, replaced);
        if is_validity {
            ValidityPoolMetrics::record_admission(replaced.is_some());
        }
        Ok(outcome)
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

    fn partition_hashes_by_pool(&self, hashes: Vec<TxHash>) -> (Vec<TxHash>, Vec<TxHash>) {
        let nonce_pool = self.nonce_pool.read();
        let mut protocol_hashes = Vec::with_capacity(hashes.len());
        let mut sidecar_hashes = Vec::new();

        for hash in hashes {
            if nonce_pool.contains(&hash) {
                sidecar_hashes.push(hash);
            } else {
                protocol_hashes.push(hash);
            }
        }

        (protocol_hashes, sidecar_hashes)
    }

    async fn add_sidecar_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: T,
    ) -> PoolResult<AddedTransactionOutcome> {
        let validated = self.validator().validate_transaction(origin, transaction).await;
        self.add_validated_sidecar_transaction(validated, origin)
    }

    fn add_validated_sidecar_transaction(
        &self,
        validated: TransactionValidationOutcome<T>,
        origin: TransactionOrigin,
    ) -> PoolResult<AddedTransactionOutcome> {
        // Capture the block-expiry bound before `validated` is consumed by the
        // match below, mirroring the protocol admission paths. A sidecar (EIP-8130)
        // transaction carrying block-number validity predicates must be recorded
        // in the block-expiry index so it is evicted once its last valid block
        // passes; a finite 2D-nonce-channel sidecar has no time-based expiry, so
        // without this it would linger indefinitely.
        let block_expiry_bound = Self::validity_block_expiry_bound(&validated);
        let flashblock_expiry_bound = Self::validity_flashblock_expiry_bound(&validated);
        let _admission_guard = self.protocol_admission_lock.lock();
        if let Some(transaction) = validated.as_valid_transaction() {
            self.ensure_not_published_expired(*transaction.transaction().hash(), &validated)?;
        }
        match validated {
            TransactionValidationOutcome::Valid {
                transaction,
                propagate,
                authorities,
                state_nonce,
                ..
            } => {
                // Keep the sidecar lock order consistent everywhere: nonce_pool before listeners.
                let mut nonce_pool = self.nonce_pool.write();
                let mut listeners = self.listeners.write();
                let validated = self.validated_pool_transaction(
                    transaction,
                    origin,
                    propagate,
                    authorities,
                    &mut nonce_pool,
                );
                let generation = validated
                    .transaction
                    .limit_class()
                    .map(|class| class.classification_generation);
                let admission = Self::admission_for(&validated.transaction);
                let is_validity = !validated.transaction.validity_predicates().is_empty();
                let outcome = nonce_pool.insert_validated(validated, state_nonce)?;
                // nonce_pool serializes sidecar replacement. Never acquire it while holding guard.
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
                let hash = outcome.outcome.hash;
                let rejection = if !current {
                    Some(Self::stale_classification_error(hash))
                } else if let (Some(replaced), Some(admission)) = (&outcome.replaced, &admission) {
                    guard
                        .check_replacement(replaced.hash(), admission)
                        .err()
                        .map(|rejection| Self::limit_rejection_error(hash, rejection))
                } else {
                    None
                };
                if let Some(error) = rejection {
                    let removed = nonce_pool.remove_transactions(&[hash]);
                    listeners.on_discarded(&removed);
                    listeners.on_lane_moved(&outcome.anchor);
                    if let Some(replaced) = &outcome.replaced {
                        let restored = ValidPoolTransaction {
                            transaction_id: replaced.transaction_id,
                            transaction: replaced.transaction.clone(),
                            propagate: replaced.propagate,
                            timestamp: replaced.timestamp,
                            origin: replaced.origin,
                            authority_ids: replaced.authority_ids.clone(),
                        };
                        if nonce_pool.insert_validated(restored, state_nonce).is_err() {
                            guard.release(replaced.hash());
                            listeners.on_discarded(std::slice::from_ref(replaced));
                        }
                    }
                    return Err(error);
                }
                match (&outcome.replaced, admission) {
                    (Some(replaced), Some(admission)) => {
                        guard.release(replaced.hash());
                        guard.insert_forced(admission);
                    }
                    (Some(replaced), None) => {
                        guard.release(replaced.hash());
                    }
                    (None, Some(admission)) => {
                        if let Err(rejection) = guard.try_admit(admission) {
                            let hash = outcome.outcome.hash;
                            let removed = nonce_pool.remove_transactions(&[hash]);
                            listeners.on_discarded(&removed);
                            listeners.on_lane_moved(&outcome.anchor);
                            return Err(Self::limit_rejection_error(hash, rejection));
                        }
                    }
                    (None, None) => {}
                }
                drop(guard);
                listeners.on_inserted(&nonce_pool, &outcome);
                if is_validity {
                    ValidityPoolMetrics::record_admission(outcome.replaced.is_some());
                }
                // Record the block-expiry bound after a successful insertion,
                // dropping the replaced hash's stale entry in the same pass.
                // Release the sidecar locks first so the block-expiry index is
                // not acquired while holding the nonce pool.
                let inserted_hash = outcome.outcome.hash;
                let replaced_hash = outcome.replaced.as_ref().map(|replaced| *replaced.hash());
                drop(listeners);
                drop(nonce_pool);
                self.register_block_expiry(
                    inserted_hash,
                    block_expiry_bound,
                    flashblock_expiry_bound,
                    replaced_hash,
                );
                Ok(outcome.outcome)
            }
            TransactionValidationOutcome::Invalid(transaction, error) => {
                Err(reth_transaction_pool::error::PoolError::new(
                    *transaction.hash(),
                    reth_transaction_pool::error::PoolErrorKind::InvalidTransaction(error),
                ))
            }
            TransactionValidationOutcome::Error(hash, error) => {
                Err(reth_transaction_pool::error::PoolError::other(hash, error.to_string()))
            }
        }
    }

    fn validated_pool_transaction(
        &self,
        transaction: reth_transaction_pool::validate::ValidTransaction<T>,
        origin: TransactionOrigin,
        propagate: bool,
        authorities: Option<Vec<Address>>,
        nonce_pool: &mut TwoDNoncePool<T>,
    ) -> ValidPoolTransaction<T> {
        let transaction = transaction.into_transaction();
        let sender_id = nonce_pool.sender_id_or_create(transaction.sender());
        let authority_ids = authorities.map(|authorities| {
            authorities
                .into_iter()
                .map(|authority| nonce_pool.sender_id_or_create(authority))
                .collect()
        });

        ValidPoolTransaction {
            transaction_id: reth_transaction_pool::identifier::TransactionId::new(
                sender_id,
                transaction.nonce(),
            ),
            transaction,
            propagate,
            timestamp: std::time::Instant::now(),
            origin,
            authority_ids,
        }
    }

    fn merged_pending_listener(&self, kind: TransactionListenerKind) -> mpsc::Receiver<TxHash> {
        let protocol = self.protocol_pool.pending_transactions_listener_for(kind);
        let sidecar = self.listeners.write().subscribe_pending(kind);
        merge_receivers(protocol, sidecar)
    }

    fn merged_new_transactions_listener(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        let protocol = self.protocol_pool.new_transactions_listener_for(kind);
        let sidecar = self.listeners.write().subscribe_new_transactions(kind);
        merge_receivers(protocol, sidecar)
    }

    fn merged_all_transactions_listener(&self) -> AllTransactionsEvents<T> {
        let mut protocol = self.protocol_pool.all_transactions_event_listener();
        let mut sidecar = self.listeners.write().subscribe_all();
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        spawn(async move {
            let mut protocol_open = true;
            let mut sidecar_open = true;
            while protocol_open || sidecar_open {
                tokio::select! {
                    event = protocol.next(), if protocol_open => match event {
                        Some(event) => {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        None => protocol_open = false,
                    },
                    event = sidecar.next(), if sidecar_open => match event {
                        Some(event) => {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        None => sidecar_open = false,
                    }
                }
            }
        });
        AllTransactionsEvents::new(rx)
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
        let nonce_pool = self.nonce_pool.read();
        let (pending, queued) = nonce_pool.pending_and_queued_txn_count();
        let pending_size: usize =
            nonce_pool.pending_transactions().iter().map(|tx| tx.encoded_length()).sum();
        let queued_size: usize =
            nonce_pool.queued_transactions().iter().map(|tx| tx.encoded_length()).sum();
        size.pending += pending;
        size.pending_size += pending_size;
        size.queued += queued;
        size.queued_size += queued_size;
        size.total += pending + queued;
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
        if !self.is_sidecar_transaction(&transaction) {
            let hash = *transaction.hash();
            let sender = transaction.sender();
            let nonce = transaction.nonce();
            let validated = self.validator().validate_transaction(origin, transaction).await;
            let _admission_guard = self.protocol_admission_lock.lock();
            self.ensure_protocol_classification_current(hash, &validated)?;
            self.ensure_not_published_expired(hash, &validated)?;
            let replaced = self.protocol_replacement_hash(sender, nonce);
            let pre_admitted = self.pre_admit_protocol_transaction(hash, replaced, &validated)?;
            // Capture the block-expiry bound and validity-lane flag before
            // `validated` is consumed below.
            let block_expiry_bound = Self::validity_block_expiry_bound(&validated);
            let flashblock_expiry_bound = Self::validity_flashblock_expiry_bound(&validated);
            let is_validity = Self::has_validity_predicates(&validated);
            let events =
                match self.protocol_pool.inner().add_transaction_and_subscribe(origin, validated) {
                    Ok(events) => events,
                    Err(error) => {
                        if pre_admitted {
                            self.guard.write().release(&hash);
                        }
                        return Err(error);
                    }
                };
            self.gate_protocol_admission(hash, replaced, pre_admitted)?;
            self.register_block_expiry(hash, block_expiry_bound, flashblock_expiry_bound, replaced);
            if is_validity {
                ValidityPoolMetrics::record_admission(replaced.is_some());
            }
            return Ok(events);
        }

        let hash = *transaction.hash();
        let (events, listener) = self.listeners.write().subscribe_hash(hash);
        if let Err(error) = self.add_sidecar_transaction(origin, transaction).await {
            self.listeners.write().unsubscribe_hash_listener(&hash, &listener);
            return Err(error);
        }
        Ok(events)
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        if self.is_sidecar_transaction(&transaction) {
            self.add_sidecar_transaction(origin, transaction).await
        } else {
            let hash = *transaction.hash();
            let sender = transaction.sender();
            let nonce = transaction.nonce();
            let validated = self.validator().validate_transaction(origin, transaction).await;
            self.add_validated_protocol_transaction(origin, hash, sender, nonce, validated)
        }
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let mut results = Vec::with_capacity(transactions.len());
        for transaction in transactions {
            results.push(self.add_transaction(origin, transaction).await);
        }
        results
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let mut results = Vec::with_capacity(transactions.len());
        for (origin, transaction) in transactions {
            results.push(self.add_transaction(origin, transaction).await);
        }
        results
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        self.protocol_pool.transaction_event_listener(tx_hash).or_else(|| {
            self.nonce_pool
                .read()
                .contains(&tx_hash)
                .then(|| self.listeners.write().subscribe_hash(tx_hash).0)
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
        hashes.extend(
            self.nonce_pool
                .read()
                .all_transactions()
                .into_iter()
                .filter(|transaction| transaction.propagate)
                .map(|transaction| *transaction.hash()),
        );
        hashes
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.pooled_transaction_hashes_max(max);
        if hashes.len() >= max {
            return hashes;
        }

        let nonce_pool = self.nonce_pool.read();
        for transaction in nonce_pool.all_transactions() {
            if transaction.propagate {
                hashes.push(*transaction.hash());
                if hashes.len() >= max {
                    break;
                }
            }
        }
        hashes
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions();
        transactions.extend(
            self.nonce_pool
                .read()
                .all_transactions()
                .into_iter()
                .filter(|transaction| transaction.propagate),
        );
        transactions
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions_max(max);
        if transactions.len() >= max {
            return transactions;
        }

        let nonce_pool = self.nonce_pool.read();
        for transaction in nonce_pool.all_transactions() {
            if transaction.propagate {
                transactions.push(transaction);
                if transactions.len() >= max {
                    break;
                }
            }
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
            self.nonce_pool
                .read()
                .get(&tx_hash)
                .and_then(|transaction| transaction.transaction.clone().try_into_pooled().ok())
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
        let base_fee = best_transactions_attributes.basefee;
        Box::new(MergeBestTransactions::new(
            self.protocol_pool.best_transactions_with_attributes(best_transactions_attributes),
            Box::new(self.nonce_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            self.ordering.clone(),
            base_fee,
        ))
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        let base_fee = best_transactions_attributes.basefee;
        Box::new(MergeBestTransactions::new(
            self.protocol_pool.best_transactions_with_attributes(best_transactions_attributes),
            Box::new(self.nonce_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            self.ordering.clone(),
            base_fee,
        ))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions();
        transactions.extend(self.nonce_pool.read().pending_transactions());
        transactions
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // Channelized nonce sequences and nonce-free replay ids live in separate
        // namespaces from account nonces, so this sender+nonce lookup
        // intentionally remains protocol-only.
        self.protocol_pool.get_pending_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions_max(max);
        if transactions.len() >= max {
            return transactions;
        }

        let remaining = max - transactions.len();
        transactions
            .extend(self.nonce_pool.read().pending_transactions().into_iter().take(remaining));
        transactions
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.queued_transactions();
        transactions.extend(self.nonce_pool.read().queued_transactions());
        transactions
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (pending, queued) = self.protocol_pool.pending_and_queued_txn_count();
        let (sidecar_pending, sidecar_queued) =
            self.nonce_pool.read().pending_and_queued_txn_count();
        (pending + sidecar_pending, queued + sidecar_queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        let mut transactions = self.protocol_pool.all_transactions();
        let nonce_pool = self.nonce_pool.read();
        transactions.pending.extend(nonce_pool.pending_transactions());
        transactions.queued.extend(nonce_pool.queued_transactions());
        transactions
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.all_transaction_hashes();
        hashes.extend(self.nonce_pool.read().all_hashes());
        hashes
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.remove_transactions(protocol_hashes);
        self.release_from_guard(&removed);
        let sidecar_removed = self.nonce_pool.write().remove_transactions(&sidecar_hashes);
        if !sidecar_removed.is_empty() {
            self.listeners.write().on_discarded(&sidecar_removed);
        }
        self.release_from_guard(&sidecar_removed);
        removed.extend(sidecar_removed);
        removed
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.remove_transactions_and_descendants(protocol_hashes);
        self.release_from_guard(&removed);
        let sidecar_removed =
            self.nonce_pool.write().remove_transactions_and_descendants(&sidecar_hashes);
        if !sidecar_removed.is_empty() {
            self.listeners.write().on_discarded(&sidecar_removed);
        }
        self.release_from_guard(&sidecar_removed);
        removed.extend(sidecar_removed);
        removed
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut removed = self.protocol_pool.remove_transactions_by_sender(sender);
        self.release_from_guard(&removed);
        let sidecar_removed = self.nonce_pool.write().remove_transactions_by_sender(sender);
        if !sidecar_removed.is_empty() {
            self.listeners.write().on_discarded(&sidecar_removed);
        }
        self.release_from_guard(&sidecar_removed);
        removed.extend(sidecar_removed);
        removed
    }

    fn prune_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.prune_transactions(protocol_hashes);
        self.release_from_guard(&removed);
        let pruned = {
            let mut nonce_pool = self.nonce_pool.write();
            let pruned = nonce_pool.prune_mined(&sidecar_hashes);
            self.listeners.write().on_lane_update(&pruned.lanes);
            pruned
        };
        self.release_from_guard(&pruned.removed);
        self.release_from_guard(&pruned.lanes.discarded);
        removed.extend(pruned.removed);
        removed.extend(pruned.lanes.discarded);
        removed
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.protocol_pool.retain_unknown(announcement);
        if announcement.is_empty() {
            return;
        }

        let nonce_pool = self.nonce_pool.read();
        announcement.retain_by_hash(|hash| !nonce_pool.contains(hash));
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        let nonce_pool = self.nonce_pool.read();
        announcement.retain_by_hash(|hash| {
            self.protocol_pool.get(hash).is_some() || nonce_pool.contains(hash)
        });
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get(tx_hash).or_else(|| self.nonce_pool.read().get(tx_hash))
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let nonce_pool = self.nonce_pool.read();
        txs.into_iter()
            .filter_map(|tx| self.protocol_pool.get(&tx).or_else(|| nonce_pool.get(&tx)))
            .collect()
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        let nonce_pool = self.nonce_pool.read();
        let protocol_txs = PropagatedTransactions(
            txs.0.into_iter().filter(|(hash, _)| !nonce_pool.contains(hash)).collect(),
        );
        drop(nonce_pool);
        self.protocol_pool.on_propagated(protocol_txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_sender(sender);
        transactions.extend(self.nonce_pool.read().transactions_by_sender(sender));
        transactions
    }

    fn get_pending_transactions_with_predicate(
        &self,
        mut predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions =
            self.protocol_pool.get_pending_transactions_with_predicate(&mut predicate);
        transactions.extend(
            self.nonce_pool
                .read()
                .pending_transactions()
                .into_iter()
                .filter(|transaction| predicate(transaction)),
        );
        transactions
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_pending_transactions_by_sender(sender);
        transactions.extend(self.nonce_pool.read().pending_transactions_by_sender(sender));
        transactions
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_queued_transactions_by_sender(sender);
        transactions.extend(self.nonce_pool.read().queued_transactions_by_sender(sender));
        transactions
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_origin(origin);
        transactions.extend(
            self.nonce_pool
                .read()
                .all_transactions()
                .into_iter()
                .filter(|transaction| transaction.origin == origin),
        );
        transactions
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_pending_transactions_by_origin(origin);
        transactions.extend(
            self.nonce_pool
                .read()
                .pending_transactions()
                .into_iter()
                .filter(|transaction| transaction.origin == origin),
        );
        transactions
    }

    fn unique_senders(&self) -> AddressSet {
        let mut senders = self.protocol_pool.unique_senders();
        for sender in self.nonce_pool.read().unique_senders() {
            senders.insert(sender);
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
        indices_bitarray: B128,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v4(versioned_hashes, indices_bitarray)
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
        let merged = MergeBestTransactions::new(
            self.protocol_pool.best_transactions_with_attributes(attributes),
            Box::new(self.nonce_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            self.ordering.clone(),
            base_fee,
        );
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
        {
            let mut nonce_pool = self.nonce_pool.write();
            let pruned = nonce_pool.prune_mined(&mined_transactions);
            // The nonce-free validity window is in milliseconds, evaluated
            // against `block.timestamp * 1000`.
            let expired = nonce_pool.remove_expired_nonce_free(now.saturating_mul(1_000));
            let mut listeners = self.listeners.write();
            if !pruned.removed.is_empty() {
                listeners.on_mined(&pruned.removed, block_hash);
            }
            listeners.on_lane_update(&pruned.lanes);
            if !expired.is_empty() {
                listeners.on_discarded(&expired);
            }
            // Sidecar maintenance lock order is nonce_pool -> listeners -> guard.
            let mut guard = self.guard.write();
            for transaction in pruned.removed.iter().chain(&pruned.lanes.discarded) {
                guard.release(transaction.hash());
            }
            for transaction in &expired {
                guard.release(transaction.hash());
            }
        }
        self.expire_due_buckets(now);
        self.expire_by_block(block_number);
        self.reconcile_guard();
        GuardMetrics::tracked().set(self.guard.read().len() as f64);
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        // Serialize balance invalidation with protocol pre-admission so a
        // dropped reservation cannot be recreated from an older validation
        // snapshot after reth publishes the transaction.
        let _admission_guard = self.protocol_admission_lock.lock();
        let dropped = {
            let mut guard = self.guard.write();
            let mut dropped = Vec::new();
            for account in &accounts {
                dropped.extend(guard.on_balance_changed(account.address, account.balance));
            }
            dropped
        };
        let invalidated = self.remove_dropped_across_pools(dropped);
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
        self.protocol_pool.update_accounts(accounts);
        let discarded: Vec<_> = discard_candidates
            .into_iter()
            .filter(|hash| self.protocol_pool.get(hash).is_none())
            .collect();
        let mut guard = self.guard.write();
        for hash in discarded {
            guard.release(&hash);
        }
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

    fn on_inserted(&mut self, nonce_pool: &TwoDNoncePool<T>, outcome: &InsertOutcome<T>) {
        let hash = outcome.outcome.hash;
        let Some(transaction) = nonce_pool.get(&hash) else {
            return;
        };

        if let Some(replaced) = &outcome.replaced {
            self.broadcast_hash_event(replaced.hash(), TransactionEvent::Replaced(hash));
            self.broadcast_all(FullTransactionEvent::Replaced {
                transaction: Arc::clone(replaced),
                replaced_by: hash,
            });
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
        self.all_events.retain(|listener| listener.try_send(event.clone()).is_ok());
    }

    fn broadcast_pending_transaction(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let hash = *transaction.hash();
        self.broadcast_hash_event(&hash, TransactionEvent::Pending);
        self.broadcast_all(FullTransactionEvent::Pending(hash));
        self.broadcast_pending(transaction);
        self.broadcast_new(NewTransactionEvent::pending(Arc::clone(transaction)));
    }

    fn broadcast_pending(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        self.pending_all.retain(|listener| listener.try_send(*transaction.hash()).is_ok());
        if transaction.propagate {
            self.pending_propagate
                .retain(|listener| listener.try_send(*transaction.hash()).is_ok());
        }
    }

    fn broadcast_new(&mut self, event: NewTransactionEvent<T>) {
        self.new_all.retain(|listener| listener.try_send(event.clone()).is_ok());
        if event.transaction.propagate {
            self.new_propagate.retain(|listener| listener.try_send(event.clone()).is_ok());
        }
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
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BaseBlock, BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives,
        BaseTxEnvelope, Eip8130Constants, Eip8130Signed, TxEip8130,
    };
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use base_test_utils::build_test_genesis_everest;
    use futures::{FutureExt, StreamExt, future::join_all};
    use reth_primitives_traits::SealedBlock;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_tasks::Runtime;
    use reth_transaction_pool::{
        CanonicalStateUpdate, PoolConfig, PoolUpdateKind, PriceBumpConfig, TransactionOrigin,
        blobstore::InMemoryBlobStore, identifier::TransactionId,
        validate::EthTransactionValidatorBuilder,
    };

    use super::*;
    use crate::{
        BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, LimitClass, ValidityOperator,
        ValidityPredicate, WatchSet,
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
        build_integration_pool_at(false)
    }

    /// [`build_integration_pool`] with the Keystore active (Zenith at genesis).
    fn build_zenith_integration_pool()
    -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
        build_integration_pool_at(true)
    }

    fn build_integration_pool_at(
        zenith: bool,
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
            .build_with_tasks(Runtime::test(), blob_store.clone())
            .map(|inner| {
                // Seed a realistic wall clock so millisecond-scale nonce-free
                // `valid_before` bounds evaluate against a representative `now`
                // (default is timestamp 0, where no future ms bound is reachable).
                // Reuse the same seam the live head uses: `update_l1_block_info`
                // with `tx = None` stores only the header timestamp and leaves the
                // L1 block info untouched, so the harness shares the one admission
                // clock writer rather than a second, unsynchronized setter.
                let validator =
                    BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
                        .require_l1_data_gas_fee(false);
                validator.update_l1_block_info::<_, TxEip1559>(
                    &alloy_consensus::Header {
                        timestamp: INTEGRATION_POOL_NOW_SECS,
                        ..Default::default()
                    },
                    None,
                );
                validator
            });
        let ordering = BaseOrdering::default();
        let pool = Pool::new(validator, ordering.clone(), blob_store, PoolConfig::default());
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
        let tx = TxEip1559 {
            chain_id: test_chain_id(),
            nonce,
            gas_limit: 50_000,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas: 0,
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
    async fn protocol_validity_transaction_replaces_with_only_a_higher_max_fee() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let predicate =
            ValidityPredicate::BlockNumber { op: ValidityOperator::LessThan, value: U256::from(2) };
        let original = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_000)
            .with_validity_predicates(vec![predicate.clone()]);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        let replacement = self_paid_eoa_8130(&signer, U256::ZERO, 0, 0, 1_001)
            .with_validity_predicates(vec![predicate]);
        let replacement_hash = *replacement.hash();
        pool.add_transaction(TransactionOrigin::Local, replacement)
            .await
            .expect("validity replacement only requires a higher max fee");

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
        let mut over_events = sidecar_pool.listeners.write().subscribe_hash(over_hash).0;
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
        assert!(admitted.iter().all(|hash| pool.nonce_pool.read().contains(hash)));

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
    async fn stale_sidecar_replacement_restores_original_transaction() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let original = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        let replacement = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_250);
        let replacement_hash = *replacement.hash();
        let mut replacement_events = pool.listeners.write().subscribe_hash(replacement_hash).0;
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::Local, replacement).await;
        pool.validator().validator().clear_limit_class_cache();

        let error = pool
            .add_validated_sidecar_transaction(validated, TransactionOrigin::Local)
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
        let mut queued_events = pool.listeners.write().subscribe_hash(queued_hash).0;

        let successor = self_paid_eoa_8130(&signer, U256::from(1), 2, 0, 1_000);
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

        pool.add_validated_sidecar_transaction(validated, TransactionOrigin::Local)
            .expect_err("stale classification must roll back the insert");

        let pending = pool.nonce_pool.read().pending_transactions();
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
        let mut successor_events = pool.listeners.write().subscribe_hash(*lane[1].hash()).0;
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
            .nonce_pool
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
        let mut head_events = pool.listeners.write().subscribe_hash(*lane[2].hash()).0;

        let removed = pool.apply_state_diff(&[nonce_diff(1)]);

        assert!(removed.is_empty());
        let pending: Vec<_> = pool
            .nonce_pool
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
        assert!(pool.nonce_pool.read().contains(&hash));
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
        assert!(!pool.nonce_pool.read().contains(&hash));
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
        pool.nonce_pool.write().insert_validated(valid_pool_transaction(transaction), 0).unwrap();
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
}
