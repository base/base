//! Base transaction-pool wrapper that combines the protocol pool with a 2D nonce sidecar and a
//! separate validity transaction store.

use std::{collections::HashMap, fmt, sync::Arc};

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
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool, PoolResult, PoolSize,
    PoolTransaction, PoolUpdateKind, PropagatedTransactions, SubPool, TransactionEvents,
    TransactionListenerKind, TransactionOrigin, TransactionPool, TransactionPoolExt,
    TransactionValidationOutcome, TransactionValidationTaskExecutor, TransactionValidator,
    ValidPoolTransaction,
    error::{Eip7702PoolTransactionError, InvalidPoolTransactionError},
    pool::{AddedTransactionState, QueuedReason, TransactionEvent},
};
use tokio::{
    spawn,
    sync::mpsc::{self, error::TrySendError},
};
use tracing::debug;

use crate::{
    Admission, BasePooledTx, BaseTransactionValidator, BestTransactionLane, FlashblockExpiry,
    GuardLimits, GuardMetrics, InvalidationCause, InvalidationKey, LimitRejection, MempoolGuard,
    ParkableBestTransactions, ParkableTransactionPool, ParkedBestTransactions, PoolAdmissionError,
    PredicateContext, StateDiffInvalidation, TransactionClass, ValidityPoolConfig,
    ValidityPoolMetrics,
    best::MergeBestTransactions,
    two_d_nonce_pool::{
        InsertContext, InsertOutcome, LaneId, LaneUpdate, NoncePoolMode, SidecarLimits,
        TwoDNoncePool,
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

/// Wrapper around reth's transaction pool that adds a 2D nonce sidecar for EIP-8130 channels
/// and a separate store for validity transactions.
///
/// Every transaction carrying validity predicates lives in the validity store, with its own
/// capacity and admission guard, so validity traffic never evicts or rate-limits normal
/// transactions and normal traffic never evicts validity ones. Within one sequential lane
/// `(sender, nonce_key)` all pooled transactions share one class: an addition of the other class
/// is rejected with [`PoolAdmissionError::LaneConflict`], except for a fee-bumped replacement of
/// a lone same-nonce incumbent, which moves the lane to the new class.
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
    /// Every transaction carrying validity predicates, in every nonce shape.
    validity_pool: Arc<RwLock<TwoDNoncePool<T>>>,
    /// Event listeners for both custom stores.
    listeners: Arc<RwLock<SidecarListeners<T>>>,
    /// Admission and invalidation ledger for normal EIP-8130 transactions.
    guard: Arc<RwLock<MempoolGuard>>,
    /// Independent admission and invalidation ledger for validity EIP-8130 transactions.
    validity_guard: Arc<RwLock<MempoolGuard>>,
    /// Block and flashblock deadline index for validity transactions, including
    /// EIP-1559 advanced submissions.
    block_expiry: Arc<RwLock<crate::BlockExpiryIndex>>,
    /// Highest published position on the current branch; checked under `protocol_admission_lock`.
    published_flashblock: Arc<RwLock<Option<PredicateContext>>>,
    /// Serializes pool admission with successful flashblock publication and
    /// deadline eviction, so a same-nonce submission cannot race the sweep.
    /// Validation itself remains outside this lock.
    protocol_admission_lock: Arc<Mutex<()>>,
    /// EIP-7702 authorities of protocol-pool transactions, recorded at admission under
    /// `protocol_admission_lock` and forgotten when the facade removes the transaction. Reth's
    /// internal evictions are not reported, so hashes it no longer holds are also dropped when
    /// their authority is read and on every canonical update.
    protocol_authorizations: Arc<Mutex<crate::ProtocolAuthorizations>>,
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
            validity_pool: Arc::clone(&self.validity_pool),
            listeners: Arc::clone(&self.listeners),
            guard: Arc::clone(&self.guard),
            validity_guard: Arc::clone(&self.validity_guard),
            block_expiry: Arc::clone(&self.block_expiry),
            published_flashblock: Arc::clone(&self.published_flashblock),
            protocol_admission_lock: Arc::clone(&self.protocol_admission_lock),
            protocol_authorizations: Arc::clone(&self.protocol_authorizations),
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
        let config = protocol_pool.config();
        let nonce_pool = TwoDNoncePool::new(config.price_bumps)
            .with_limits(SidecarLimits::from_pool_config(config));
        let validity_pool =
            Self::new_validity_pool(&protocol_pool, ValidityPoolConfig::from_pool_config(config));
        Self {
            protocol_pool,
            ordering,
            nonce_pool: Arc::new(RwLock::new(nonce_pool)),
            validity_pool: Arc::new(RwLock::new(validity_pool)),
            listeners: Arc::new(RwLock::new(SidecarListeners::default())),
            guard: Arc::new(RwLock::new(MempoolGuard::unlimited())),
            validity_guard: Arc::new(RwLock::new(MempoolGuard::unlimited())),
            block_expiry: Arc::new(RwLock::new(crate::BlockExpiryIndex::new())),
            published_flashblock: Arc::new(RwLock::new(None)),
            protocol_admission_lock: Arc::new(Mutex::new(())),
            protocol_authorizations: Arc::new(Mutex::new(Default::default())),
        }
    }

    fn new_validity_pool(
        protocol_pool: &Pool<
            TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>,
            O,
            S,
        >,
        validity: ValidityPoolConfig,
    ) -> TwoDNoncePool<T> {
        let config = protocol_pool.config();
        let block_info = protocol_pool.block_info();
        let mut pool = TwoDNoncePool::new(config.price_bumps)
            .with_mode(NoncePoolMode::Validity)
            .with_limits(SidecarLimits::for_validity(config, &validity));
        pool.set_block_info(block_info.block_gas_limit, block_info.pending_basefee);
        pool
    }

    /// Configures EIP-8130 admission limits. The normal and validity stores each get an
    /// independent ledger with these limits. Call before sharing the pool.
    #[must_use]
    pub fn with_guard_limits(self, limits: GuardLimits) -> Self {
        *self.guard.write() = MempoolGuard::new(limits);
        *self.validity_guard.write() = MempoolGuard::new(limits);
        self
    }

    /// Configures the validity store's capacity. Call before sharing the pool.
    #[must_use]
    pub fn with_validity_pool_config(self, config: ValidityPoolConfig) -> Self {
        *self.validity_pool.write() = Self::new_validity_pool(&self.protocol_pool, config);
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

    /// Returns whether a transaction is stored in a custom store (the validity store or the
    /// EIP-8130 sidecar) rather than the protocol pool.
    fn is_custom_store_transaction(transaction: &T) -> bool {
        TransactionClass::is_validity(transaction) || transaction.is_eip8130_sidecar_transaction()
    }

    /// Returns the custom store and guard that own transactions of `class`.
    fn custom_store(
        &self,
        class: TransactionClass,
    ) -> (&RwLock<TwoDNoncePool<T>>, &RwLock<MempoolGuard>) {
        match class {
            TransactionClass::Normal => (&self.nonce_pool, &self.guard),
            TransactionClass::Validity => (&self.validity_pool, &self.validity_guard),
        }
    }

    fn custom_stores(&self) -> [&RwLock<TwoDNoncePool<T>>; 2] {
        [&self.nonce_pool, &self.validity_pool]
    }

    fn guards(&self) -> [&RwLock<MempoolGuard>; 2] {
        [&self.guard, &self.validity_guard]
    }

    fn custom_get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.nonce_pool.read().get(hash).or_else(|| self.validity_pool.read().get(hash))
    }

    /// Merges the normal stores (protocol pool and sidecar) with the validity store into one
    /// stream ordered by complete priority; normal transactions win equal-priority ties.
    fn merged_best_transactions(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> MergeBestTransactions<T, O> {
        let base_fee = attributes.basefee;
        let normal = MergeBestTransactions::new(
            self.protocol_pool.best_transactions_with_attributes(attributes),
            Box::new(self.nonce_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            T::is_eip8130_sidecar_transaction,
            self.ordering.clone(),
            base_fee,
        );
        MergeBestTransactions::new(
            Box::new(normal),
            Box::new(self.validity_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            TransactionClass::is_validity::<T>,
            self.ordering.clone(),
            base_fee,
        )
    }

    /// Re-ranks validity key-zero lanes against the protocol pool's current block gas limit
    /// and pending base fee, announcing transactions whose pending state changed.
    fn sync_validity_block_info(&self) {
        let block_info = self.protocol_pool.block_info();
        let update = self
            .validity_pool
            .write()
            .set_block_info(block_info.block_gas_limit, block_info.pending_basefee);
        self.listeners.write().on_lane_moved(&update);
    }

    /// Applies a canonical update to the validity store: prunes mined transactions, moves
    /// key-zero cursors to the changed accounts' nonces (backward only on a reorg), expires
    /// nonce-free entries and re-ranks lanes against the new block.
    fn maintain_validity_store(
        &self,
        block_hash: B256,
        mined_transactions: &[TxHash],
        changed_accounts: &[(Address, Option<U256>, Option<u64>)],
        is_reorg: bool,
    ) {
        let mut validity_pool = self.validity_pool.write();
        let pruned = validity_pool.prune_mined(mined_transactions);
        let accounts = validity_pool.apply_account_updates(changed_accounts, is_reorg);
        // `protocol_pool.on_canonical_state_change` has already advanced the
        // validator's head clock and block info to this block.
        let expired = validity_pool
            .remove_expired_nonce_free(self.validator().validator().block_timestamp_ms());
        let block_info = self.protocol_pool.block_info();
        let fees =
            validity_pool.set_block_info(block_info.block_gas_limit, block_info.pending_basefee);
        // Validity store maintenance lock order is validity_pool -> listeners -> guard.
        let mut listeners = self.listeners.write();
        if !pruned.removed.is_empty() {
            listeners.on_mined(&pruned.removed, block_hash);
        }
        listeners.on_lane_update(&pruned.lanes);
        listeners.on_lane_update(&accounts);
        listeners.on_discarded(&expired);
        listeners.on_lane_moved(&fees);
        let mut guard = self.validity_guard.write();
        for transaction in pruned
            .removed
            .iter()
            .chain(&pruned.lanes.discarded)
            .chain(&accounts.discarded)
            .chain(&expired)
        {
            guard.release(transaction.hash());
        }
        drop(guard);
        drop(listeners);
        drop(validity_pool);
        // Mined hashes already left the block-expiry index with the canonical update.
        self.forget_block_expiry(
            pruned
                .lanes
                .discarded
                .iter()
                .chain(&accounts.discarded)
                .chain(&expired)
                .map(|transaction| *transaction.hash()),
        );
    }

    fn record_validity_store_size(&self) {
        let validity_pool = self.validity_pool.read();
        ValidityPoolMetrics::record_store_size(validity_pool.len(), validity_pool.size());
    }

    /// Publishes a custom store's lane changes and releases discarded transactions from `guard`.
    fn publish_lane_update(&self, update: &LaneUpdate<T>, guard: &RwLock<MempoolGuard>) {
        self.listeners.write().on_lane_update(update);
        if !update.discarded.is_empty() {
            let mut guard = guard.write();
            for transaction in &update.discarded {
                guard.release(transaction.hash());
            }
        }
        self.forget_block_expiry(update.discarded.iter().map(|transaction| *transaction.hash()));
    }

    /// Enforces lane exclusivity for `transaction`, called under `protocol_admission_lock`.
    /// A nonce-free transaction's lane is its replay id.
    ///
    /// Returns the opposite-class incumbent a same-slot cross-class addition replaces. That is
    /// allowed only when the incumbent is the sole pooled transaction in the lane and the
    /// addition meets its replacement price bump; any other addition to a lane holding the other
    /// class fails with [`PoolAdmissionError::LaneConflict`]. A hash already pooled in the other
    /// class is [`PoolErrorKind::AlreadyImported`]. Only the transaction's own lane is read.
    ///
    /// [`PoolErrorKind::AlreadyImported`]: reth_transaction_pool::error::PoolErrorKind::AlreadyImported
    fn cross_class_incumbent(
        &self,
        transaction: &T,
    ) -> PoolResult<Option<Arc<ValidPoolTransaction<T>>>> {
        let hash = *transaction.hash();
        let class = TransactionClass::of(transaction);
        let imported = match class {
            TransactionClass::Normal => self.validity_pool.read().contains(&hash),
            TransactionClass::Validity => {
                self.nonce_pool.read().contains(&hash) || self.protocol_pool.get(&hash).is_some()
            }
        };
        if imported {
            return Err(reth_transaction_pool::error::PoolError::new(
                hash,
                reth_transaction_pool::error::PoolErrorKind::AlreadyImported,
            ));
        }
        let Some(lane) = BestTransactionLane::for_pooled_transaction(transaction) else {
            // A nonce-free transaction's replay id is its slot: the same rules apply to a
            // replay-id incumbent in the other class's store.
            let incumbent = transaction.eip8130_replay_id().and_then(|replay_id| {
                let opposite = match class {
                    TransactionClass::Normal => &self.validity_pool,
                    TransactionClass::Validity => &self.nonce_pool,
                };
                opposite.read().nonce_free_transaction(&replay_id)
            });
            return match incumbent {
                Some(incumbent)
                    if incumbent.transaction.is_replacement_underpriced(
                        transaction,
                        &self.protocol_pool.config().price_bumps,
                    ) =>
                {
                    Err(reth_transaction_pool::error::PoolError::new(
                        hash,
                        reth_transaction_pool::error::PoolErrorKind::ReplacementUnderpriced,
                    ))
                }
                incumbent => Ok(incumbent),
            };
        };
        let lane_id = (lane.sender, lane.nonce_key);
        let occupants = match class {
            TransactionClass::Normal => self.validity_pool.read().lane_transactions(&lane_id),
            TransactionClass::Validity if lane.nonce_key.is_zero() => {
                self.protocol_pool.get_transactions_by_sender(lane.sender)
            }
            TransactionClass::Validity => self.nonce_pool.read().lane_transactions(&lane_id),
        };
        match occupants.as_slice() {
            [] => Ok(None),
            [incumbent] if incumbent.nonce() == transaction.nonce() => {
                if incumbent.transaction.is_replacement_underpriced(
                    transaction,
                    &self.protocol_pool.config().price_bumps,
                ) {
                    return Err(reth_transaction_pool::error::PoolError::new(
                        hash,
                        reth_transaction_pool::error::PoolErrorKind::ReplacementUnderpriced,
                    ));
                }
                Ok(Some(Arc::clone(incumbent)))
            }
            _ => Err(PoolAdmissionError::LaneConflict.into_pool_error(hash)),
        }
    }

    /// Removes the incumbent a cross-class replacement superseded, after the replacement was
    /// stored. Its listeners receive a terminal `Discarded`, unlike a same-store replacement's
    /// `Replaced`, and its guard record is released.
    fn remove_cross_class_incumbent(&self, incumbent: &Arc<ValidPoolTransaction<T>>) {
        let hash = *incumbent.hash();
        let removed = self.remove_dropped_across_pools(vec![hash]);
        self.release_from_guard(&removed);
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
        let accounts: Vec<_> =
            diffs.iter().map(|diff| (diff.address, diff.balance, diff.nonce)).collect();
        let mut removed = Vec::new();
        let mut cursor_discards = Vec::new();
        let mut dropped = Vec::new();
        for class in [TransactionClass::Normal, TransactionClass::Validity] {
            let (store, guard) = self.custom_store(class);
            // Protocol nonces move validity key-zero lane cursors even when the transaction
            // that advanced them was never pooled here; the sidecar holds no key-zero lanes.
            let update = store.write().apply_account_updates(&accounts, false);
            self.publish_lane_update(&update, guard);
            cursor_discards.extend(update.discarded);
            let (behind, lane_moves) = self.resolve_nonce_advances(guard, store, &advances);
            // Move lane cursors before releasing what fell behind them, so a
            // successor that stays pending is not re-announced as promoted.
            if !lane_moves.is_empty() {
                let mut update = LaneUpdate::default();
                {
                    let mut store = store.write();
                    for (lane_id, nonce) in lane_moves {
                        update.extend(store.advance_lane_nonce(lane_id, nonce));
                    }
                }
                self.publish_lane_update(&update, guard);
                removed.extend(update.discarded);
            }
            let mut guard = guard.write();
            dropped.extend(guard.invalidate_exact(exact.clone()));
            dropped.extend(behind.into_iter().filter(|hash| guard.release(hash)));
            for diff in diffs {
                if let Some(balance) = diff.balance {
                    dropped.extend(guard.on_balance_changed(diff.address, balance));
                }
            }
        }
        removed.extend(self.remove_dropped_across_pools(dropped));
        // A guarded EIP-8130 transaction the account update discarded is invalidated too; the
        // standard validity transactions it discards were never guarded.
        let guarded_cursor_discards = cursor_discards
            .iter()
            .filter(|transaction| transaction.transaction.as_eip8130().is_some())
            .count();
        let invalidated = removed.len() + guarded_cursor_discards;
        GuardMetrics::record_state_diff_invalidations(invalidated);
        if invalidated > 0 {
            debug!(count = invalidated, "EIP-8130 transactions invalidated by state diff");
        }
        removed.extend(cursor_discards);
        self.record_validity_store_size();
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
        let mut dropped = self.guard.write().invalidate_all();
        dropped.extend(self.validity_guard.write().invalidate_all());
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
        guard: &RwLock<MempoolGuard>,
        store: &RwLock<TwoDNoncePool<T>>,
        advances: &[(InvalidationKey, u64)],
    ) -> (Vec<TxHash>, Vec<(LaneId, u64)>) {
        let mut behind = Vec::new();
        let mut lane_moves = Vec::new();
        if advances.is_empty() {
            return (behind, lane_moves);
        }
        let watchers: Vec<_> = {
            let guard = guard.read();
            advances.iter().map(|(key, nonce)| (key, *nonce, guard.watchers(key))).collect()
        };
        let nonce_pool = store.read();
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
        let [protocol_hashes, sidecar_hashes, validity_hashes] =
            self.partition_hashes_by_pool(dropped);
        let mut removed = if protocol_hashes.is_empty() {
            Vec::new()
        } else {
            self.protocol_pool.remove_transactions(protocol_hashes)
        };
        for (store, hashes) in
            self.custom_stores().into_iter().zip([sidecar_hashes, validity_hashes])
        {
            if hashes.is_empty() {
                continue;
            }
            let store_removed = store.write().remove_transactions(&hashes);
            if !store_removed.is_empty() {
                self.listeners.write().on_discarded(&store_removed);
            }
            removed.extend(store_removed);
        }
        self.record_validity_store_size();
        self.forget_block_expiry(removed.iter().map(|transaction| *transaction.hash()));
        self.forget_protocol_authorizations(removed.iter().map(|transaction| *transaction.hash()));
        removed
    }

    /// Drops removed transactions from the block-expiry index, so it tracks only pooled
    /// transactions. Call without holding a store lock.
    fn forget_block_expiry(&self, hashes: impl IntoIterator<Item = TxHash>) {
        let mut hashes = hashes.into_iter().peekable();
        if hashes.peek().is_none() {
            return;
        }
        let mut block_expiry = self.block_expiry.write();
        for hash in hashes {
            block_expiry.remove(&hash);
        }
    }

    /// Releases removed transactions from whichever guard tracks them. Hashes are unique across
    /// stores, so releasing from both ledgers frees exactly the owning record.
    fn release_from_guard(&self, removed: &[Arc<ValidPoolTransaction<T>>]) {
        if removed.is_empty() {
            return;
        }
        for guard in self.guards() {
            let mut guard = guard.write();
            for transaction in removed {
                guard.release(transaction.hash());
            }
        }
        self.forget_block_expiry(removed.iter().map(|transaction| *transaction.hash()));
        self.forget_protocol_authorizations(removed.iter().map(|transaction| *transaction.hash()));
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
        let mut dropped = Vec::new();
        let mut bucket_count = 0;
        for guard in self.guards() {
            let (guard_dropped, guard_buckets) =
                guard.write().invalidate_expiry_buckets_through(horizon);
            dropped.extend(guard_dropped);
            bucket_count += guard_buckets;
        }
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
            if transaction.transaction().validity_conditions().is_expired(&next) {
                return Err(reth_transaction_pool::error::PoolError::other(
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
            return Err(reth_transaction_pool::error::PoolError::new(
                hash,
                reth_transaction_pool::error::PoolErrorKind::InvalidTransaction(
                    InvalidPoolTransactionError::ExceedsGasLimit(reserved_gas, block_gas_limit),
                ),
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
            .is_some_and(|transaction| !transaction.transaction().validity_conditions().is_empty())
    }

    fn reconcile_guard(&self) {
        let mut released = 0;
        for class in [TransactionClass::Normal, TransactionClass::Validity] {
            released += self.reconcile_guard_for(class);
        }
        GuardMetrics::record_reconcile_releases(released);
    }

    /// Releases records in `class`'s guard whose transaction left its stores without a release.
    fn reconcile_guard_for(&self, class: TransactionClass) -> usize {
        let (store, guard) = self.custom_store(class);
        let tracked = guard.read().tracked_hashes();
        if tracked.is_empty() {
            return 0;
        }
        let pooled = |store: &TwoDNoncePool<T>, hash: &TxHash| {
            store.contains(hash)
                || (class == TransactionClass::Normal && self.protocol_pool.get(hash).is_some())
        };
        let stale: Vec<_> = {
            let store = store.read();
            tracked.into_iter().filter(|hash| !pooled(&store, hash)).collect()
        };
        if stale.is_empty() {
            return 0;
        }

        // Recheck only stale candidates under admission serialization. The
        // initial full scan stays outside this critical section, while a
        // reserved-before-insert transaction cannot be mistaken for an orphan.
        let _admission_guard = self.protocol_admission_lock.lock();
        let stale: Vec<_> = {
            let store = store.read();
            stale.into_iter().filter(|hash| !pooled(&store, hash)).collect()
        };
        let mut guard = guard.write();
        for hash in &stale {
            guard.release(hash);
        }
        stale.len()
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
        let incumbent = self.precheck_protocol_admission(hash, &validated)?;
        let authorities = Self::protocol_authorities(&validated);
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
        self.record_protocol_authorizations(hash, replaced, authorities);
        let replaced = self.finish_protocol_cross_class(hash, replaced, incumbent);
        self.register_block_expiry(hash, block_expiry_bound, flashblock_expiry_bound, replaced);
        if is_validity {
            ValidityPoolMetrics::record_admission(replaced.is_some());
        }
        Ok(outcome)
    }

    /// Checks every protocol admission entry point runs under `protocol_admission_lock` before
    /// reth sees the transaction, so stale validation cannot replace a pooled transaction.
    /// Returns the cross-class incumbent the admission replaces.
    fn precheck_protocol_admission(
        &self,
        hash: TxHash,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<Option<Arc<ValidPoolTransaction<T>>>> {
        self.ensure_protocol_classification_current(hash, validated)?;
        self.ensure_not_published_expired(hash, validated)?;
        self.ensure_eip8130_fits_block_gas_limit(hash, validated)?;
        let incumbent = self.protocol_cross_class_incumbent(validated)?;
        self.ensure_cross_class_delegation_limits(TransactionClass::Normal, validated)?;
        Ok(incumbent)
    }

    /// The EIP-7702 authorities a validated transaction carries.
    fn protocol_authorities(validated: &TransactionValidationOutcome<T>) -> Vec<Address> {
        match validated {
            TransactionValidationOutcome::Valid { authorities: Some(authorities), .. } => {
                authorities.clone()
            }
            _ => Vec::new(),
        }
    }

    /// Records the authorities of a protocol admission once the protocol pool holds it, and
    /// forgets the protocol transaction it replaced.
    fn record_protocol_authorizations(
        &self,
        hash: TxHash,
        replaced: Option<TxHash>,
        authorities: Vec<Address>,
    ) {
        // Most admissions carry no authorization and replace nothing: skip the lookups.
        if (authorities.is_empty() && replaced.is_none()) || self.protocol_pool.get(&hash).is_none()
        {
            return;
        }
        if let Some(replaced) =
            replaced.filter(|replaced| self.protocol_pool.get(replaced).is_none())
        {
            self.forget_protocol_authorizations([replaced]);
        }
        self.protocol_authorizations
            .lock()
            .record(hash, authorities, |hash| self.protocol_pool.get(hash).is_some());
    }

    /// Drops removed protocol transactions from the authorization index.
    fn forget_protocol_authorizations(&self, hashes: impl IntoIterator<Item = TxHash>) {
        let mut hashes = hashes.into_iter().peekable();
        if hashes.peek().is_none() {
            return;
        }
        let mut protocol_authorizations = self.protocol_authorizations.lock();
        for hash in hashes {
            protocol_authorizations.forget(&hash);
        }
    }

    /// Applies the protocol pool's EIP-7702 rules across the protocol pool and the validity
    /// store, called under `protocol_admission_lock`. Each side enforces them for its own
    /// transactions; this adds what lies in the other one. A transaction may not authorize an
    /// address holding more than the delegated slot limit across both, and a protocol-nonce
    /// transaction whose sender the other side authorizes is limited to that many executable
    /// transactions without a nonce gap. Validity-store senders are checked by the store, given
    /// [`InsertContext::pending_delegation`].
    fn ensure_cross_class_delegation_limits(
        &self,
        class: TransactionClass,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<()> {
        let TransactionValidationOutcome::Valid { transaction, state_nonce, authorities, .. } =
            validated
        else {
            return Ok(());
        };
        let transaction = transaction.transaction();
        let hash = *transaction.hash();
        let limit = self.protocol_pool.config().max_inflight_delegated_slot_limit;
        let eip7702_error = |error| {
            reth_transaction_pool::error::PoolError::new(
                hash,
                reth_transaction_pool::error::PoolErrorKind::InvalidTransaction(
                    InvalidPoolTransactionError::Eip7702(error),
                ),
            )
        };
        let validity_pool = self.validity_pool.read();
        let reserved = authorities.iter().flatten().any(|authority| {
            validity_pool.sender_count(authority)
                + self.protocol_pool.get_transactions_by_sender(*authority).len()
                > limit
        });
        if reserved {
            return Err(eip7702_error(Eip7702PoolTransactionError::AuthorityReserved));
        }
        let account_lane = BestTransactionLane::for_pooled_transaction(transaction)
            .is_some_and(|lane| lane.nonce_key.is_zero());
        if class == TransactionClass::Validity
            || !account_lane
            || !validity_pool.has_pending_authorization(&transaction.sender())
        {
            return Ok(());
        }
        drop(validity_pool);
        let pending = self.protocol_pool.get_pending_transactions_by_sender(transaction.sender());
        if pending.is_empty() {
            if transaction.nonce().saturating_sub(*state_nonce) >= limit as u64 {
                return Err(eip7702_error(Eip7702PoolTransactionError::OutOfOrderTxFromDelegated));
            }
        } else if pending.iter().all(|pooled| pooled.nonce() != transaction.nonce())
            && pending.len() >= limit
        {
            return Err(eip7702_error(Eip7702PoolTransactionError::InflightTxLimitReached));
        }
        Ok(())
    }

    /// Returns whether a protocol-pool transaction authorizes `address`, dropping recorded
    /// hashes the protocol pool no longer holds.
    fn has_protocol_authorization(&self, address: &Address) -> bool {
        self.protocol_authorizations
            .lock()
            .retain_pooled_for(address, |hash| self.protocol_pool.get(hash).is_some())
    }

    fn protocol_cross_class_incumbent(
        &self,
        validated: &TransactionValidationOutcome<T>,
    ) -> PoolResult<Option<Arc<ValidPoolTransaction<T>>>> {
        validated
            .as_valid_transaction()
            .map_or(Ok(None), |transaction| self.cross_class_incumbent(transaction.transaction()))
    }

    /// Removes a cross-class incumbent once the protocol pool holds its replacement, and
    /// returns the hash the admission replaced.
    fn finish_protocol_cross_class(
        &self,
        hash: TxHash,
        replaced: Option<TxHash>,
        incumbent: Option<Arc<ValidPoolTransaction<T>>>,
    ) -> Option<TxHash> {
        let incumbent = incumbent.filter(|_| self.protocol_pool.get(&hash).is_some())?;
        self.remove_cross_class_incumbent(&incumbent);
        replaced.or_else(|| Some(*incumbent.hash()))
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

    /// Splits hashes into protocol, sidecar and validity store members. Unknown hashes go to
    /// the protocol pool, which ignores them.
    fn partition_hashes_by_pool(&self, hashes: Vec<TxHash>) -> [Vec<TxHash>; 3] {
        let nonce_pool = self.nonce_pool.read();
        let validity_pool = self.validity_pool.read();
        let mut protocol_hashes = Vec::with_capacity(hashes.len());
        let mut sidecar_hashes = Vec::new();
        let mut validity_hashes = Vec::new();

        for hash in hashes {
            if nonce_pool.contains(&hash) {
                sidecar_hashes.push(hash);
            } else if validity_pool.contains(&hash) {
                validity_hashes.push(hash);
            } else {
                protocol_hashes.push(hash);
            }
        }

        [protocol_hashes, sidecar_hashes, validity_hashes]
    }

    async fn add_custom_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: T,
    ) -> PoolResult<AddedTransactionOutcome> {
        let (store, _) = self.custom_store(TransactionClass::of(&transaction));
        let balance_epoch = store.read().balance_epoch();
        let validated = self.validator().validate_transaction(origin, transaction).await;
        self.add_validated_custom_transaction(validated, origin, balance_epoch)
    }

    /// Inserts a validated transaction into the custom store of its class: the validity store
    /// for a transaction with validity predicates, otherwise the EIP-8130 sidecar.
    /// `balance_epoch` is that store's balance epoch read before validation started.
    fn add_validated_custom_transaction(
        &self,
        validated: TransactionValidationOutcome<T>,
        origin: TransactionOrigin,
        balance_epoch: u64,
    ) -> PoolResult<AddedTransactionOutcome> {
        // Capture the block-expiry bound before `validated` is consumed by the
        // match below, mirroring the protocol admission paths. A custom-store
        // transaction carrying block-number validity predicates must be recorded
        // in the block-expiry index so it is evicted once its last valid block
        // passes; a finite 2D-nonce-channel sidecar has no time-based expiry, so
        // without this it would linger indefinitely.
        let block_expiry_bound = Self::validity_block_expiry_bound(&validated);
        let flashblock_expiry_bound = Self::validity_flashblock_expiry_bound(&validated);
        let _admission_guard = self.protocol_admission_lock.lock();
        let mut incumbent = None;
        if let Some(transaction) = validated.as_valid_transaction() {
            let hash = *transaction.transaction().hash();
            self.ensure_not_published_expired(hash, &validated)?;
            self.ensure_eip8130_fits_block_gas_limit(hash, &validated)?;
            incumbent = self.cross_class_incumbent(transaction.transaction())?;
            if TransactionClass::of(transaction.transaction()) == TransactionClass::Validity {
                self.ensure_cross_class_delegation_limits(TransactionClass::Validity, &validated)?;
            }
        }
        match validated {
            TransactionValidationOutcome::Valid {
                balance,
                transaction,
                propagate,
                state_nonce,
                bytecode_hash,
                authorities,
            } => {
                // Hashes this admission removed from the store, dropped from the block-expiry
                // index once the store locks are released.
                let mut forgotten: Vec<TxHash> = Vec::new();
                // Every exit past a successful store insertion changed the store, so a validity
                // admission refreshes the store gauges once the locks are released.
                let refresh_validity_gauges =
                    TransactionClass::of(transaction.transaction()) == TransactionClass::Validity;
                let result = 'admit: {
                    let class = TransactionClass::of(transaction.transaction());
                    let (store, guard) = self.custom_store(class);
                    let pending_delegation = class == TransactionClass::Validity
                        && self.has_protocol_authorization(&transaction.transaction().sender());
                    // Keep the custom-store lock order consistent everywhere: store before listeners.
                    let mut nonce_pool = store.write();
                    let mut listeners = self.listeners.write();
                    let validated =
                        Self::validated_pool_transaction(transaction, origin, propagate);
                    // A classification invalidation advances the generation under the admission
                    // lock, so rejecting a stale one before touching the store is atomic with
                    // it and leaves any incumbent untouched.
                    if validated.transaction.limit_class().is_some_and(|class| {
                        class.classification_generation
                            != self.validator().validator().limit_class_cache_generation()
                    }) {
                        break 'admit Err(Self::stale_classification_error(*validated.hash()));
                    }
                    let admission = Self::admission_for(&validated.transaction);
                    let is_validity = class == TransactionClass::Validity;
                    let context = InsertContext {
                        state_nonce,
                        balance: Some((balance, balance_epoch)),
                        bytecode_hash,
                        authorities,
                        pending_delegation,
                    };
                    let hash = *validated.hash();
                    let mut guard = guard.write();
                    // Settle guard capacity before touching the store, so a rejection leaves any
                    // incumbent in place. Only an incumbent holding a guard record lends its
                    // capacity to the replacement; a standard transaction holds none, so its
                    // replacement is charged like a fresh submission.
                    let guarded_incumbent = admission
                        .as_ref()
                        .and_then(|_| nonce_pool.replacement_target(&validated.transaction))
                        .map(|incumbent| *incumbent.hash())
                        .filter(|incumbent| guard.contains(incumbent));
                    let reserved = match (&admission, guarded_incumbent) {
                        (Some(admission), Some(incumbent)) => {
                            if let Err(rejection) = guard.check_replacement(&incumbent, admission) {
                                break 'admit Err(Self::limit_rejection_error(hash, rejection));
                            }
                            false
                        }
                        (Some(admission), None) => {
                            if let Err(rejection) = guard.try_admit(admission.clone()) {
                                break 'admit Err(Self::limit_rejection_error(hash, rejection));
                            }
                            true
                        }
                        (None, _) => false,
                    };
                    let mut outcome = match nonce_pool.insert(validated, context) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            if reserved {
                                guard.release(&hash);
                            }
                            break 'admit Err(error);
                        }
                    };
                    // Transactions below the anchored lane cursor are gone.
                    for transaction in &outcome.anchor.discarded {
                        guard.release(transaction.hash());
                    }
                    forgotten.extend(outcome.anchor.discarded.iter().map(|tx| *tx.hash()));
                    listeners.on_discarded(&outcome.anchor.discarded);
                    if let Some(replaced) = &outcome.replaced {
                        guard.release(replaced.hash());
                    }
                    if let (Some(admission), Some(_)) = (admission, guarded_incumbent) {
                        guard.insert_forced(admission);
                    }
                    let mut evicted = nonce_pool.evict_over_capacity();
                    for transaction in &evicted {
                        guard.release(transaction.hash());
                    }
                    drop(guard);
                    let inserted_hash = outcome.outcome.hash;
                    let survived = |moved: &Arc<ValidPoolTransaction<T>>| {
                        !evicted.iter().any(|transaction| transaction.hash() == moved.hash())
                    };
                    outcome.anchor.promoted.retain(survived);
                    outcome.anchor.queued.retain(|(moved, _)| survived(moved));
                    outcome.promoted.retain(survived);
                    outcome.demoted.retain(|(moved, _)| survived(moved));
                    if evicted.iter().any(|transaction| *transaction.hash() == inserted_hash) {
                        evicted.retain(|transaction| *transaction.hash() != inserted_hash);
                        evicted.extend(outcome.replaced);
                        forgotten.extend(evicted.iter().map(|tx| *tx.hash()));
                        listeners.on_discarded(&evicted);
                        listeners.on_lane_moved(&outcome.anchor);
                        break 'admit Err(reth_transaction_pool::error::PoolError::new(
                            inserted_hash,
                            reth_transaction_pool::error::PoolErrorKind::DiscardedOnInsert,
                        ));
                    }
                    forgotten.extend(evicted.iter().map(|tx| *tx.hash()));
                    listeners.on_discarded(&evicted);
                    listeners.on_inserted(&nonce_pool, &outcome);
                    // Record the block-expiry bound after a successful insertion,
                    // dropping the replaced hash's stale entry in the same pass.
                    // Release the store locks first so the block-expiry index is
                    // not acquired while holding the store.
                    let mut replaced_hash =
                        outcome.replaced.as_ref().map(|replaced| *replaced.hash());
                    drop(listeners);
                    drop(nonce_pool);
                    if let Some(incumbent) = incumbent {
                        self.remove_cross_class_incumbent(&incumbent);
                        replaced_hash = replaced_hash.or_else(|| Some(*incumbent.hash()));
                    }
                    if is_validity {
                        ValidityPoolMetrics::record_admission(replaced_hash.is_some());
                    }
                    self.register_block_expiry(
                        inserted_hash,
                        block_expiry_bound,
                        flashblock_expiry_bound,
                        replaced_hash,
                    );
                    Ok(outcome.outcome)
                };
                self.forget_block_expiry(forgotten);
                if refresh_validity_gauges {
                    self.record_validity_store_size();
                }
                result
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

    /// Wraps a validated sidecar transaction. Its sender and authority ids are
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
        for store in self.custom_stores() {
            let store = store.read();
            let (pending, queued) = store.pending_and_queued_txn_count();
            let pending_size: usize =
                store.pending_transactions().iter().map(|tx| tx.transaction.size()).sum();
            let queued_size: usize =
                store.queued_transactions().iter().map(|tx| tx.transaction.size()).sum();
            let (basefee, basefee_size) = store.basefee_count_and_size();
            size.pending += pending;
            size.pending_size += pending_size;
            size.basefee += basefee;
            size.basefee_size += basefee_size;
            size.queued += queued - basefee;
            size.queued_size += queued_size.saturating_sub(basefee_size);
            size.total += pending + queued;
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
        if !Self::is_custom_store_transaction(&transaction) {
            let hash = *transaction.hash();
            let sender = transaction.sender();
            let nonce = transaction.nonce();
            let validated = self.validator().validate_transaction(origin, transaction).await;
            let _admission_guard = self.protocol_admission_lock.lock();
            let incumbent = self.precheck_protocol_admission(hash, &validated)?;
            let authorities = Self::protocol_authorities(&validated);
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
            self.record_protocol_authorizations(hash, replaced, authorities);
            let replaced = self.finish_protocol_cross_class(hash, replaced, incumbent);
            self.register_block_expiry(hash, block_expiry_bound, flashblock_expiry_bound, replaced);
            if is_validity {
                ValidityPoolMetrics::record_admission(replaced.is_some());
            }
            return Ok(events);
        }

        let hash = *transaction.hash();
        let (events, listener) = self.listeners.write().subscribe_hash(hash);
        if let Err(error) = self.add_custom_transaction(origin, transaction).await {
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
        if Self::is_custom_store_transaction(&transaction) {
            self.add_custom_transaction(origin, transaction).await
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
        self.add_transactions_with_origins(
            transactions.into_iter().map(|transaction| (origin, transaction)).collect(),
        )
        .await
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        if transactions
            .iter()
            .any(|(_, transaction)| Self::is_custom_store_transaction(transaction))
        {
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
            self.custom_stores()
                .into_iter()
                .any(|store| store.read().contains(&tx_hash))
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
        for store in self.custom_stores() {
            hashes.extend(
                store
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
        if hashes.len() >= max {
            return hashes;
        }

        for store in self.custom_stores() {
            for transaction in store.read().all_transactions() {
                if transaction.propagate {
                    hashes.push(*transaction.hash());
                    if hashes.len() >= max {
                        return hashes;
                    }
                }
            }
        }
        hashes
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions();
        for store in self.custom_stores() {
            transactions.extend(
                store
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
        if transactions.len() >= max {
            return transactions;
        }

        for store in self.custom_stores() {
            for transaction in store.read().all_transactions() {
                if transaction.propagate {
                    transactions.push(transaction);
                    if transactions.len() >= max {
                        return transactions;
                    }
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
            self.custom_get(&tx_hash)
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
        self.best_transactions_with_attributes(best_transactions_attributes)
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        Box::new(self.merged_best_transactions(best_transactions_attributes))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions();
        for store in self.custom_stores() {
            transactions.extend(store.read().pending_transactions());
        }
        transactions
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // Channelized nonce sequences and nonce-free replay ids live in separate
        // namespaces from account nonces, so this sender+nonce lookup reads only
        // the protocol pool and the validity store's key-zero lane.
        self.protocol_pool.get_pending_transaction_by_sender_and_nonce(sender, nonce).or_else(
            || self.validity_pool.read().pending_lane_transaction(&(sender, U256::ZERO), nonce),
        )
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions_max(max);
        if transactions.len() >= max {
            return transactions;
        }

        for store in self.custom_stores() {
            let remaining = max - transactions.len();
            transactions.extend(store.read().pending_transactions().into_iter().take(remaining));
            if transactions.len() >= max {
                break;
            }
        }
        transactions
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.queued_transactions();
        for store in self.custom_stores() {
            transactions.extend(store.read().queued_transactions());
        }
        transactions
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (mut pending, mut queued) = self.protocol_pool.pending_and_queued_txn_count();
        for store in self.custom_stores() {
            let (store_pending, store_queued) = store.read().pending_and_queued_txn_count();
            pending += store_pending;
            queued += store_queued;
        }
        (pending, queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        let mut transactions = self.protocol_pool.all_transactions();
        for store in self.custom_stores() {
            let store = store.read();
            transactions.pending.extend(store.pending_transactions());
            transactions.queued.extend(store.queued_transactions());
        }
        transactions
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.all_transaction_hashes();
        for store in self.custom_stores() {
            hashes.extend(store.read().all_hashes());
        }
        hashes
    }

    fn all_transactions_by_sender(
        &self,
        sender: Address,
    ) -> AllPoolTransactions<Self::Transaction> {
        let mut transactions = self.protocol_pool.all_transactions_by_sender(sender);
        for store in self.custom_stores() {
            let store = store.read();
            transactions.pending.extend(store.pending_transactions_by_sender(sender));
            transactions.queued.extend(store.queued_transactions_by_sender(sender));
        }
        transactions
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let [protocol_hashes, sidecar_hashes, validity_hashes] =
            self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.remove_transactions(protocol_hashes);
        self.release_from_guard(&removed);
        for (store, hashes) in
            self.custom_stores().into_iter().zip([sidecar_hashes, validity_hashes])
        {
            let store_removed = store.write().remove_transactions(&hashes);
            if !store_removed.is_empty() {
                self.listeners.write().on_discarded(&store_removed);
            }
            self.release_from_guard(&store_removed);
            removed.extend(store_removed);
        }
        self.record_validity_store_size();
        removed
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let [protocol_hashes, sidecar_hashes, validity_hashes] =
            self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.remove_transactions_and_descendants(protocol_hashes);
        self.release_from_guard(&removed);
        for (store, hashes) in
            self.custom_stores().into_iter().zip([sidecar_hashes, validity_hashes])
        {
            let store_removed = store.write().remove_transactions_and_descendants(&hashes);
            if !store_removed.is_empty() {
                self.listeners.write().on_discarded(&store_removed);
            }
            self.release_from_guard(&store_removed);
            removed.extend(store_removed);
        }
        self.record_validity_store_size();
        removed
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut removed = self.protocol_pool.remove_transactions_by_sender(sender);
        self.release_from_guard(&removed);
        for store in self.custom_stores() {
            let store_removed = store.write().remove_transactions_by_sender(sender);
            if !store_removed.is_empty() {
                self.listeners.write().on_discarded(&store_removed);
            }
            self.release_from_guard(&store_removed);
            removed.extend(store_removed);
        }
        self.record_validity_store_size();
        removed
    }

    fn prune_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let [protocol_hashes, sidecar_hashes, validity_hashes] =
            self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.prune_transactions(protocol_hashes);
        self.release_from_guard(&removed);
        for (store, hashes) in
            self.custom_stores().into_iter().zip([sidecar_hashes, validity_hashes])
        {
            let pruned = {
                let mut store = store.write();
                let pruned = store.prune_mined(&hashes);
                self.listeners.write().on_lane_update(&pruned.lanes);
                pruned
            };
            self.release_from_guard(&pruned.removed);
            self.release_from_guard(&pruned.lanes.discarded);
            removed.extend(pruned.removed);
            removed.extend(pruned.lanes.discarded);
        }
        self.record_validity_store_size();
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
        let validity_pool = self.validity_pool.read();
        announcement
            .retain_by_hash(|hash| !nonce_pool.contains(hash) && !validity_pool.contains(hash));
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        let nonce_pool = self.nonce_pool.read();
        let validity_pool = self.validity_pool.read();
        announcement.retain_by_hash(|hash| {
            self.protocol_pool.get(hash).is_some()
                || nonce_pool.contains(hash)
                || validity_pool.contains(hash)
        });
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get(tx_hash).or_else(|| self.custom_get(tx_hash))
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let nonce_pool = self.nonce_pool.read();
        let validity_pool = self.validity_pool.read();
        txs.into_iter()
            .filter_map(|tx| {
                self.protocol_pool
                    .get(&tx)
                    .or_else(|| nonce_pool.get(&tx))
                    .or_else(|| validity_pool.get(&tx))
            })
            .collect()
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        let nonce_pool = self.nonce_pool.read();
        let validity_pool = self.validity_pool.read();
        let protocol_txs = PropagatedTransactions(
            txs.0
                .into_iter()
                .filter(|(hash, _)| !nonce_pool.contains(hash) && !validity_pool.contains(hash))
                .collect(),
        );
        drop(validity_pool);
        drop(nonce_pool);
        self.protocol_pool.on_propagated(protocol_txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_sender(sender);
        for store in self.custom_stores() {
            transactions.extend(store.read().transactions_by_sender(sender));
        }
        transactions
    }

    fn get_pending_transactions_with_predicate(
        &self,
        mut predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions =
            self.protocol_pool.get_pending_transactions_with_predicate(&mut predicate);
        for store in self.custom_stores() {
            transactions.extend(
                store
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
        for store in self.custom_stores() {
            transactions.extend(store.read().pending_transactions_by_sender(sender));
        }
        transactions
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_queued_transactions_by_sender(sender);
        for store in self.custom_stores() {
            transactions.extend(store.read().queued_transactions_by_sender(sender));
        }
        transactions
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool
            .get_highest_transaction_by_sender(sender)
            .or_else(|| self.validity_pool.read().highest_lane_transaction(&(sender, U256::ZERO)))
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool
            .get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
            .or_else(|| {
                self.validity_pool
                    .read()
                    .highest_consecutive_lane_transaction(&(sender, U256::ZERO), on_chain_nonce)
            })
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool
            .get_transaction_by_sender_and_nonce(sender, nonce)
            .or_else(|| self.validity_pool.read().lane_transaction(&(sender, U256::ZERO), nonce))
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_origin(origin);
        for store in self.custom_stores() {
            transactions.extend(
                store
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
        for store in self.custom_stores() {
            transactions.extend(
                store
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
        for store in self.custom_stores() {
            for sender in store.read().unique_senders() {
                senders.insert(sender);
            }
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
        self.protocol_pool.set_block_info(info);
        self.sync_validity_block_info();
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
            for guard in self.guards() {
                let mut guard = guard.write();
                for hash in &mined_transactions {
                    guard.release(hash);
                }
            }
            let mut block_expiry = self.block_expiry.write();
            for hash in &mined_transactions {
                block_expiry.remove(hash);
            }
        }
        let changed_accounts: Vec<_> = update
            .changed_accounts
            .iter()
            .map(|account| (account.address, Some(account.balance), Some(account.nonce)))
            .collect();
        let is_reorg = update.update_kind == PoolUpdateKind::Reorg;
        self.protocol_pool.on_canonical_state_change(update);
        self.forget_protocol_authorizations(mined_transactions.iter().copied());
        // Once per block, drop what the protocol pool evicted or dropped on its own.
        self.protocol_authorizations
            .lock()
            .retain_pooled(|hash| self.protocol_pool.get(hash).is_some());
        self.maintain_validity_store(block_hash, &mined_transactions, &changed_accounts, is_reorg);
        {
            let mut nonce_pool = self.nonce_pool.write();
            let pruned = nonce_pool.prune_mined(&mined_transactions);
            // `protocol_pool.on_canonical_state_change` has already advanced the
            // validator's head clock to this block.
            let expired = nonce_pool
                .remove_expired_nonce_free(self.validator().validator().block_timestamp_ms());
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
        GuardMetrics::tracked()
            .set((self.guard.read().len() + self.validity_guard.read().len()) as f64);
        self.record_validity_store_size();
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        // Serialize balance invalidation with protocol pre-admission so a
        // dropped reservation cannot be recreated from an older validation
        // snapshot after reth publishes the transaction.
        let _admission_guard = self.protocol_admission_lock.lock();
        // A reloaded account is authoritative, so it may move validity key-zero
        // cursors backward as well as forward.
        let account_updates: Vec<_> = accounts
            .iter()
            .map(|account| (account.address, Some(account.balance), Some(account.nonce)))
            .collect();
        for class in [TransactionClass::Normal, TransactionClass::Validity] {
            let (store, guard) = self.custom_store(class);
            let update = store.write().apply_account_updates(&account_updates, true);
            self.publish_lane_update(&update, guard);
        }
        let dropped = {
            let mut dropped = Vec::new();
            for guard in self.guards() {
                let mut guard = guard.write();
                for account in &accounts {
                    dropped.extend(guard.on_balance_changed(account.address, account.balance));
                }
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
        for hash in &discarded {
            guard.release(hash);
        }
        drop(guard);
        self.forget_protocol_authorizations(discarded);
        self.record_validity_store_size();
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
/// Event listeners for transactions in the 2D nonce sidecar.
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
                // Like the protocol pool, a transaction only its fee cap parks is in the base
                // fee subpool.
                let subpool = if matches!(reason, QueuedReason::InsufficientBaseFee) {
                    SubPool::BaseFee
                } else {
                    SubPool::Queued
                };
                self.broadcast_new(NewTransactionEvent { subpool, transaction });
            }
        }

        self.on_lane_moved(&outcome.anchor);
        for promoted in &outcome.promoted {
            self.broadcast_pending_transaction(promoted);
        }
        for (demoted, reason) in &outcome.demoted {
            self.broadcast_queued(demoted.hash(), reason.clone());
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
        for (queued, reason) in &update.queued {
            self.broadcast_queued(queued.hash(), reason.clone());
        }
    }

    fn broadcast_queued(&mut self, hash: &TxHash, reason: QueuedReason) {
        self.broadcast_hash_event(hash, TransactionEvent::Queued);
        self.broadcast_all(FullTransactionEvent::Queued(*hash, Some(reason)));
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
        SignableTransaction, Transaction, TxEip1559, TxEip7702,
        transaction::{Recovered, SignerRecoverable},
    };
    use alloy_eips::{
        eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M, eip2718::Encodable2718, eip7702::Authorization,
    };
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
        assert!(pool.nonce_pool.read().contains(&sidecar_hash));
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
        let mut over_events = sidecar_pool.listeners.write().subscribe_hash(over_hash).0;
        let error = sidecar_pool
            .add_transaction(TransactionOrigin::Local, over)
            .await
            .expect_err("sidecar transaction above the cap must be rejected");
        assert!(error.to_string().contains("sender EIP-8130 signature limit"));
        assert!(sidecar_pool.get(&over_hash).is_none());
        // Rejected before it reached the store, the transaction was never announced.
        assert!(over_events.next().now_or_never().is_none());
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
        let mut incumbent_events = pool.listeners.write().subscribe_hash(incumbent_hash).0;

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
        let mut pending = pool.listeners.write().subscribe_pending(TransactionListenerKind::All);

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
    async fn stale_sidecar_replacement_keeps_original_transaction() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let original = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_000);
        let original_hash = *original.hash();
        pool.add_transaction(TransactionOrigin::Local, original).await.unwrap();

        let replacement = self_paid_eoa_8130(&signer, U256::from(1), 0, 0, 1_250);
        let replacement_hash = *replacement.hash();
        let mut replacement_events = pool.listeners.write().subscribe_hash(replacement_hash).0;
        let balance_epoch = pool.nonce_pool.read().balance_epoch();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::Local, replacement).await;
        pool.validator().validator().clear_limit_class_cache();

        let error = pool
            .add_validated_custom_transaction(validated, TransactionOrigin::Local, balance_epoch)
            .expect_err("stale classification must reject the replacement");
        assert!(error.to_string().contains("classification changed during validation"));
        assert!(pool.get(&replacement_hash).is_none());
        assert!(pool.get(&original_hash).is_some());
        assert!(pool.guard.read().contains(&original_hash));
        assert!(!pool.guard.read().contains(&replacement_hash));
        // Rejected before it reached the store, the replacement was never announced.
        assert!(replacement_events.next().now_or_never().is_none());
    }

    #[tokio::test]
    async fn stale_insert_leaves_the_anchored_lane_untouched() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let queued = self_paid_eoa_8130(&signer, U256::from(1), 1, 0, 1_000);
        let queued_hash = *queued.hash();
        pool.add_transaction(TransactionOrigin::Local, queued).await.unwrap();
        let mut queued_events = pool.listeners.write().subscribe_hash(queued_hash).0;

        let successor = self_paid_eoa_8130(&signer, U256::from(1), 2, 0, 1_000);
        let balance_epoch = pool.nonce_pool.read().balance_epoch();
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

        pool.add_validated_custom_transaction(validated, TransactionOrigin::Local, balance_epoch)
            .expect_err("stale classification must reject the insert");

        // The stale validation's state view never reaches the store: the lane stays queued.
        let pending = pool.nonce_pool.read().pending_transactions();
        assert!(pending.iter().all(|transaction| *transaction.hash() != queued_hash));
        assert!(queued_events.next().now_or_never().is_none());
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
        let mut queued_events = pool.listeners.write().subscribe_hash(queued_hash).0;

        let successor = self_paid_eoa_8130(&lane_signer, U256::from(1), 2, 0, 1_000);
        let balance_epoch = pool.nonce_pool.read().balance_epoch();
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
            .add_validated_custom_transaction(validated, TransactionOrigin::External, balance_epoch)
            .expect_err("the cheapest tail is the inserted transaction");
        assert!(matches!(error.kind, PoolErrorKind::DiscardedOnInsert));

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
        let balance_epoch = pool.nonce_pool.read().balance_epoch();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::External, transaction).await;
        set_balance(pool, sender, balance);
        pool.add_validated_custom_transaction(
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
            // A transaction with validity predicates is guarded by the validity store's guard.
            assert_eq!(pool.validity_guard.read().contains(&hash), remains_live);
            assert!(!pool.guard.read().contains(&hash));
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

    /// A 1559 transaction carrying a block-number validity predicate that holds for any test head.
    fn validity_1559(
        signer: &PrivateKeySigner,
        nonce: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_1559_with_fees(signer, nonce, max_fee_per_gas, max_priority_fee_per_gas)
            .with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1_000_000),
            }])
    }

    fn assert_lane_conflict(result: PoolResult<AddedTransactionOutcome>) {
        let error = result.expect_err("opposite-class lane addition must be rejected");
        assert_eq!(
            PoolAdmissionError::from_pool_error(&error),
            Some(&PoolAdmissionError::LaneConflict)
        );
    }

    /// Pending and queued nonces of `sender`'s protocol-nonce lane, sorted.
    fn lane_nonces(pool: &IntegrationPool, sender: Address) -> (Vec<u64>, Vec<u64>) {
        let snapshot = pool.all_transactions_by_sender(sender);
        let nonces = |transactions: &[Arc<ValidPoolTransaction<BasePooledTransaction>>]| {
            let mut nonces: Vec<_> = transactions.iter().map(|tx| tx.nonce()).collect();
            nonces.sort_unstable();
            nonces
        };
        (nonces(&snapshot.pending), nonces(&snapshot.queued))
    }

    fn canonical_update(
        pool: &IntegrationPool,
        number: u64,
        accounts: Vec<ChangedAccount>,
        update_kind: PoolUpdateKind,
    ) {
        let block = SealedBlock::seal_slow(BaseBlock {
            header: alloy_consensus::Header { number, ..Default::default() },
            body: Default::default(),
        });
        pool.on_canonical_state_change(CanonicalStateUpdate {
            new_tip: &block,
            pending_block_base_fee: pool.block_info().pending_basefee,
            pending_block_blob_fee: None,
            changed_accounts: accounts,
            mined_transactions: Vec::new(),
            update_kind,
        });
    }

    #[tokio::test]
    async fn validity_transaction_is_stored_apart_from_normal_pools() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        fund(&client, sender.address());

        let transaction = validity_1559(&sender, 0, 1_000, 0);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();

        assert!(pool.validity_pool.read().contains(&hash));
        assert!(pool.protocol_pool.get(&hash).is_none());
        assert!(pool.get(&hash).is_some());
        assert_eq!(pool.pool_size().pending, 1);
        assert!(is_best(&pool, hash));
        assert_eq!(
            pool.get_pending_transaction_by_sender_and_nonce(sender.address(), 0)
                .map(|tx| *tx.hash()),
            Some(hash)
        );
    }

    #[tokio::test]
    async fn validity_and_normal_capacity_never_evict_each_other() {
        let mut config = PoolConfig::default();
        config.pending_limit.max_txs = 1;
        let (pool, client) = build_integration_pool_with_config(config);
        let pool = pool.with_validity_pool_config(ValidityPoolConfig {
            max_count: 1,
            max_size_bytes: usize::MAX,
            max_account_slots: 16,
        });
        let [normal_a, normal_b, validity_a, validity_b] = std::array::from_fn(|_| signer());
        for account in [&normal_a, &normal_b, &validity_a, &validity_b] {
            fund(&client, account.address());
        }

        let normal = signed_1559_with_fees(&normal_a, 0, 1_000, 0);
        let normal_hash = *normal.hash();
        pool.add_transaction(TransactionOrigin::External, normal).await.unwrap();
        pool.add_transaction(TransactionOrigin::External, validity_1559(&validity_a, 0, 1_000, 0))
            .await
            .unwrap();

        // A full validity store sheds validity transactions only.
        let _ = pool
            .add_transaction(TransactionOrigin::External, validity_1559(&validity_b, 0, 2_000, 1))
            .await;
        assert!(pool.get(&normal_hash).is_some());
        let validity_survivors = pool.validity_pool.read().all_hashes();
        assert_eq!(validity_survivors.len(), 1);

        // A full normal pool sheds normal transactions only.
        let _ = pool
            .add_transaction(
                TransactionOrigin::External,
                signed_1559_with_fees(&normal_b, 0, 2_000, 1),
            )
            .await;
        assert_eq!(pool.validity_pool.read().all_hashes(), validity_survivors);
        assert_eq!(pool.protocol_pool.pool_size().total, 1);
    }

    #[tokio::test]
    async fn opposite_class_addition_to_a_lane_is_rejected() {
        let (pool, client) = build_integration_pool();
        let [normal_first, validity_first] = std::array::from_fn(|_| signer());
        fund(&client, normal_first.address());
        fund(&client, validity_first.address());

        pool.add_transaction(TransactionOrigin::External, signed_1559(&normal_first, 0))
            .await
            .unwrap();
        assert_lane_conflict(
            pool.add_transaction(
                TransactionOrigin::External,
                validity_1559(&normal_first, 1, 1_000, 0),
            )
            .await,
        );

        pool.add_transaction(
            TransactionOrigin::External,
            validity_1559(&validity_first, 0, 1_000, 0),
        )
        .await
        .unwrap();
        assert_lane_conflict(
            pool.add_transaction(TransactionOrigin::External, signed_1559(&validity_first, 1))
                .await,
        );

        // A 2D-nonce lane is its own lane: a validity channel beside a normal account nonce lane
        // is allowed.
        pool.add_transaction(
            TransactionOrigin::External,
            signed_channel_tx(&normal_first, U256::from(1), 0, 1_000).with_validity_predicates(
                vec![ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(1_000_000),
                }],
            ),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fee_bumped_replacement_switches_a_sole_incumbent_lane_class() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        fund(&client, sender.address());

        let normal = signed_1559_with_fees(&sender, 0, 1_000, 0);
        let normal_hash = *normal.hash();
        pool.add_transaction(TransactionOrigin::External, normal).await.unwrap();

        // Without the price bump the incumbent stays.
        let error = pool
            .add_transaction(TransactionOrigin::External, validity_1559(&sender, 0, 1_001, 0))
            .await
            .unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::ReplacementUnderpriced));
        assert!(pool.get(&normal_hash).is_some());

        let validity = validity_1559(&sender, 0, 2_000, 1_000);
        let validity_hash = *validity.hash();
        pool.add_transaction(TransactionOrigin::External, validity).await.unwrap();
        assert!(pool.get(&normal_hash).is_none());
        assert!(pool.validity_pool.read().contains(&validity_hash));

        // And back: a bumped normal transaction replaces the lone validity incumbent.
        let normal = signed_1559_with_fees(&sender, 0, 4_000, 2_000);
        let normal_hash = *normal.hash();
        pool.add_transaction(TransactionOrigin::External, normal).await.unwrap();
        assert!(pool.get(&validity_hash).is_none());
        assert!(pool.protocol_pool.get(&normal_hash).is_some());

        // A same-nonce replacement is a conflict once the lane holds more than the incumbent.
        pool.add_transaction(TransactionOrigin::External, signed_1559(&sender, 1)).await.unwrap();
        assert_lane_conflict(
            pool.add_transaction(
                TransactionOrigin::External,
                validity_1559(&sender, 0, 8_000, 4_000),
            )
            .await,
        );
    }

    /// Key-zero validity lanes rank like the protocol pool: each scenario runs the same nonce
    /// sequence through both stores and compares pending and queued nonces.
    #[tokio::test]
    async fn validity_account_nonce_lane_readiness_matches_protocol_pool() {
        let fee_per_tx = U256::from(50_000u64 * 1_000);
        let scenarios: [(&str, U256, Vec<u64>); 3] = [
            ("contiguous and funded", U256::from(1_000_000_000_000u64), vec![0, 1, 2]),
            ("nonce gap", U256::from(1_000_000_000_000u64), vec![0, 2, 3]),
            ("cumulative cost exceeds balance", fee_per_tx * U256::from(2), vec![0, 1, 2]),
        ];
        for (scenario, balance, nonces) in scenarios {
            let mut observed = Vec::new();
            for validity in [false, true] {
                let (pool, client) = build_integration_pool();
                let sender = signer();
                client.add_account(sender.address(), ExtendedAccount::new(0, balance));
                for nonce in &nonces {
                    let transaction = if validity {
                        validity_1559(&sender, *nonce, 1_000, 0)
                    } else {
                        signed_1559_with_fees(&sender, *nonce, 1_000, 0)
                    };
                    pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
                }
                observed.push(lane_nonces(&pool, sender.address()));
            }
            assert_eq!(observed[0], observed[1], "{scenario}");
        }
    }

    #[tokio::test]
    async fn validity_account_nonce_lane_follows_block_fee_and_canonical_nonce_like_protocol_pool()
    {
        let mut observed = Vec::new();
        for validity in [false, true] {
            let (pool, client) = build_integration_pool();
            let sender = signer();
            let address = sender.address();
            let balance = U256::from(1_000_000_000_000u64);
            client.add_account(address, ExtendedAccount::new(0, balance));
            for nonce in 0..3 {
                let transaction = if validity {
                    validity_1559(&sender, nonce, 1_000, 0)
                } else {
                    signed_1559_with_fees(&sender, nonce, 1_000, 0)
                };
                pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
            }
            let mut steps = vec![lane_nonces(&pool, address)];

            // A pending base fee above every max fee parks the lane; lowering it restores it.
            pool.set_block_info(BlockInfo { pending_basefee: 2_000, ..pool.block_info() });
            steps.push(lane_nonces(&pool, address));
            pool.set_block_info(BlockInfo { pending_basefee: 0, ..pool.block_info() });
            steps.push(lane_nonces(&pool, address));

            // The canonical nonce advances past a transaction this node never saw mined.
            canonical_update(
                &pool,
                1,
                vec![ChangedAccount { address, nonce: 1, balance }],
                PoolUpdateKind::Commit,
            );
            steps.push(lane_nonces(&pool, address));

            observed.push(steps);

            if validity {
                // A reorg rolls the canonical nonce back, opening a gap below the lane. The
                // validity store queues the lane until the gap fills; the protocol pool keeps
                // the stale pending flags it computed before the rollback.
                canonical_update(
                    &pool,
                    1,
                    vec![ChangedAccount { address, nonce: 0, balance }],
                    PoolUpdateKind::Reorg,
                );
                assert_eq!(lane_nonces(&pool, address), (vec![], vec![1, 2]));
                assert!(!pool.best_transactions().any(|tx| tx.sender() == address));
            }
        }
        assert_eq!(observed[0], observed[1]);
        assert_eq!(observed[1][3], (vec![1, 2], vec![]));
    }

    #[tokio::test]
    async fn builder_merges_validity_and_normal_by_priority_and_invalidates_owning_lane() {
        let (pool, client) = build_integration_pool();
        let [normal, validity] = std::array::from_fn(|_| signer());
        fund(&client, normal.address());
        fund(&client, validity.address());

        let normal_tx = signed_1559_with_fees(&normal, 0, 1_000, 1);
        let normal_hash = *normal_tx.hash();
        pool.add_transaction(TransactionOrigin::External, normal_tx).await.unwrap();
        let validity_head = validity_1559(&validity, 0, 1_000, 500);
        let validity_head_hash = *validity_head.hash();
        pool.add_transaction(TransactionOrigin::External, validity_head).await.unwrap();
        pool.add_transaction(TransactionOrigin::External, validity_1559(&validity, 1, 1_000, 500))
            .await
            .unwrap();

        let mut best = pool.best_transactions();
        let first = best.next().expect("higher-tip validity transaction");
        assert_eq!(*first.hash(), validity_head_hash);
        best.mark_invalid(&first, InvalidPoolTransactionError::Underpriced);
        let rest: Vec<_> = best.map(|tx| *tx.hash()).collect();
        assert_eq!(rest, vec![normal_hash]);
    }

    /// Queued events a validity listener received so far, by hash.
    fn drain_queued_events(
        events: &mut AllTransactionsEvents<BasePooledTransaction>,
    ) -> Vec<(TxHash, Option<QueuedReason>)> {
        let mut queued = Vec::new();
        while let Some(Some(event)) = events.next().now_or_never() {
            if let FullTransactionEvent::Queued(hash, reason) = event {
                queued.push((hash, reason));
            }
        }
        queued
    }

    #[tokio::test]
    async fn validity_lane_reports_basefee_subpool_and_real_queued_reasons() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        fund(&client, sender.address());
        pool.set_block_info(BlockInfo { pending_basefee: 500, ..pool.block_info() });
        let mut new_events =
            pool.listeners.write().subscribe_new_transactions(TransactionListenerKind::All);
        let mut all_events = pool.listeners.write().subscribe_all();

        let head = validity_1559(&sender, 0, 1_000, 0);
        let head_hash = *head.hash();
        let parked = validity_1559(&sender, 1, 400, 0);
        let parked_hash = *parked.hash();
        pool.add_transaction(TransactionOrigin::External, head).await.unwrap();
        pool.add_transaction(TransactionOrigin::External, parked).await.unwrap();

        // A funded, contiguous transaction blocked only by its fee cap is in the base fee
        // subpool, as the protocol pool reports it.
        let size = pool.pool_size();
        assert_eq!((size.pending, size.basefee, size.queued), (1, 1, 0));
        let subpools: Vec<_> =
            std::iter::from_fn(|| new_events.try_recv().ok()).map(|event| event.subpool).collect();
        assert_eq!(subpools, vec![SubPool::Pending, SubPool::BaseFee]);
        assert_eq!(
            drain_queued_events(&mut all_events),
            vec![(parked_hash, Some(QueuedReason::InsufficientBaseFee))]
        );

        // A base fee rise parks the head for its fee cap, not for a nonce gap.
        pool.set_block_info(BlockInfo { pending_basefee: 2_000, ..pool.block_info() });
        assert_eq!(
            drain_queued_events(&mut all_events),
            vec![(head_hash, Some(QueuedReason::InsufficientBaseFee))]
        );
        let size = pool.pool_size();
        assert_eq!((size.pending, size.basefee, size.queued), (0, 1, 1));

        // Once fees allow both, a balance covering only the head parks its successor for funds.
        pool.set_block_info(BlockInfo { pending_basefee: 0, ..pool.block_info() });
        drain_queued_events(&mut all_events);
        set_balance(&pool, sender.address(), U256::from(50_000u64 * 1_000));
        assert_eq!(
            drain_queued_events(&mut all_events),
            vec![(parked_hash, Some(QueuedReason::InsufficientBalance))]
        );
    }

    #[tokio::test]
    async fn validity_removals_leave_the_block_expiry_index() {
        let (pool, client) = build_integration_pool();
        let pool = pool.with_validity_pool_config(ValidityPoolConfig {
            max_count: 1,
            max_size_bytes: usize::MAX,
            max_account_slots: 16,
        });
        let [first, second, third] = std::array::from_fn(|_| signer());
        for account in [&first, &second, &third] {
            fund(&client, account.address());
        }

        // Capacity eviction: the store keeps one transaction and the index tracks only it.
        pool.add_transaction(TransactionOrigin::External, validity_1559(&first, 0, 1_000, 0))
            .await
            .unwrap();
        let _ = pool
            .add_transaction(TransactionOrigin::External, validity_1559(&second, 0, 2_000, 1))
            .await;
        assert_eq!(pool.validity_pool.read().len(), 1);
        assert_eq!(pool.block_expiry.read().len(), 1);

        // Explicit removal.
        let survivor = pool.validity_pool.read().all_hashes();
        pool.remove_transactions(survivor);
        assert!(pool.block_expiry.read().is_empty());

        // A canonical nonce advance past a pooled transaction this node never saw mined.
        pool.add_transaction(TransactionOrigin::External, validity_1559(&third, 0, 1_000, 0))
            .await
            .unwrap();
        assert_eq!(pool.block_expiry.read().len(), 1);
        canonical_update(
            &pool,
            1,
            vec![ChangedAccount {
                address: third.address(),
                nonce: 1,
                balance: U256::from(1_000_000_000_000u64),
            }],
            PoolUpdateKind::Commit,
        );
        assert_eq!(pool.validity_pool.read().len(), 0);
        assert!(pool.block_expiry.read().is_empty());
    }

    #[tokio::test]
    async fn nonce_free_replay_id_replacement_spans_validity_classes() {
        let (pool, client) = build_zenith_integration_pool();
        let signer = signer();
        fund(&client, signer.address());
        let expiry = INTEGRATION_POOL_NOW_MS + 1;
        let with_predicate = |transaction: BasePooledTransaction| {
            transaction.with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1_000_000),
            }])
        };

        let normal = self_paid_eoa_8130(&signer, Eip8130Constants::NONCE_KEY_MAX, 0, expiry, 1_000);
        let normal_hash = *normal.hash();
        pool.add_transaction(TransactionOrigin::Local, normal).await.unwrap();

        // The same replay id with predicates must pay the replacement bump.
        let underpriced = with_predicate(self_paid_eoa_8130(
            &signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            expiry,
            1_001,
        ));
        let error = pool.add_transaction(TransactionOrigin::Local, underpriced).await.unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::ReplacementUnderpriced));
        assert!(pool.get(&normal_hash).is_some());

        // A bumped one replaces the incumbent, so only one transaction holds the replay id.
        let validity = with_predicate(self_paid_eoa_8130(
            &signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            expiry,
            2_000,
        ));
        let validity_hash = *validity.hash();
        pool.add_transaction(TransactionOrigin::Local, validity).await.unwrap();
        assert!(pool.validity_pool.read().contains(&validity_hash));
        assert!(pool.get(&normal_hash).is_none());
        assert!(!pool.guard.read().contains(&normal_hash));
    }

    /// The flashblock builder advances accounts intra-block before its next iterator; an
    /// admission validated against the trailing canonical state must not undo that.
    #[tokio::test]
    async fn validity_admission_keeps_an_intra_block_account_advance() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        let address = sender.address();
        let balance = U256::from(1_000_000_000_000_000_000u64);
        fund(&client, address);
        for nonce in 0..2 {
            pool.add_transaction(
                TransactionOrigin::External,
                validity_1559(&sender, nonce, 1_000, 0),
            )
            .await
            .unwrap();
        }

        pool.update_accounts(vec![ChangedAccount { address, nonce: 1, balance }]);
        let next = validity_1559(&sender, 2, 1_000, 0);
        let next_hash = *next.hash();
        pool.add_transaction(TransactionOrigin::External, next).await.unwrap();

        assert_eq!(lane_nonces(&pool, address), (vec![1, 2], vec![]));
        assert!(is_best(&pool, next_hash));
    }

    /// An EIP-7702 transaction from `signer` authorizing `authority` to delegate its code.
    fn signed_7702(
        signer: &PrivateKeySigner,
        nonce: u64,
        authority: &PrivateKeySigner,
    ) -> BasePooledTransaction {
        signed_7702_with_tip(signer, nonce, authority, 0)
    }

    /// [`signed_7702`] paying `max_priority_fee_per_gas`.
    fn signed_7702_with_tip(
        signer: &PrivateKeySigner,
        nonce: u64,
        authority: &PrivateKeySigner,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let authorization = Authorization {
            chain_id: U256::from(test_chain_id()),
            address: Address::repeat_byte(0xDD),
            nonce: 0,
        };
        let authorization_signature =
            authority.sign_hash_sync(&authorization.signature_hash()).unwrap();
        let tx = TxEip7702 {
            chain_id: test_chain_id(),
            nonce,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas,
            to: Address::repeat_byte(0xEE),
            value: U256::ZERO,
            access_list: Default::default(),
            authorization_list: vec![authorization.into_signed(authorization_signature)],
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip7702(tx.into_signed(signature));
        let recovered = envelope.clone().try_into_recovered().unwrap();
        BasePooledTransaction::new(recovered, envelope.encode_2718_len())
    }

    fn assert_eip7702_error(
        result: PoolResult<AddedTransactionOutcome>,
        expected: Eip7702PoolTransactionError,
    ) {
        let error = result.expect_err("EIP-7702 rules must reject the transaction");
        assert!(
            matches!(
                &error.kind,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Eip7702(error))
                    if std::mem::discriminant(error) == std::mem::discriminant(&expected)
            ),
            "unexpected error {error:?}"
        );
    }

    /// Admits `transaction` through `add_transaction`, or `add_transaction_and_subscribe` when
    /// `subscribed`.
    async fn admit(
        pool: &IntegrationPool,
        transaction: BasePooledTransaction,
        subscribed: bool,
    ) -> PoolResult<AddedTransactionOutcome> {
        let hash = *transaction.hash();
        if subscribed {
            pool.add_transaction_and_subscribe(TransactionOrigin::External, transaction)
                .await
                .map(|_| AddedTransactionOutcome { hash, state: AddedTransactionState::Pending })
        } else {
            pool.add_transaction(TransactionOrigin::External, transaction).await
        }
    }

    #[tokio::test]
    async fn eip7702_delegation_limits_span_validity_classes() {
        for subscribed in [false, true] {
            let (pool, client) = build_integration_pool();
            let [authority, sponsor, delegated, validity_sponsor, validity_delegated] =
                std::array::from_fn(|_| signer());
            for account in
                [&authority, &sponsor, &delegated, &validity_sponsor, &validity_delegated]
            {
                fund(&client, account.address());
            }

            // A normal transaction may not authorize an address the validity store fills.
            for nonce in 0..2 {
                admit(&pool, validity_1559(&authority, nonce, 1_000, 0), subscribed).await.unwrap();
            }
            assert_eip7702_error(
                admit(&pool, signed_7702(&sponsor, 0, &authority), subscribed).await,
                Eip7702PoolTransactionError::AuthorityReserved,
            );

            // A protocol-pool authorization limits the authority's validity transactions.
            admit(&pool, signed_7702(&sponsor, 0, &delegated), subscribed).await.unwrap();
            admit(&pool, validity_1559(&delegated, 0, 1_000, 0), subscribed).await.unwrap();
            assert_eip7702_error(
                admit(&pool, validity_1559(&delegated, 1, 1_000, 0), subscribed).await,
                Eip7702PoolTransactionError::InflightTxLimitReached,
            );

            // A validity-store authorization limits the authority's normal transactions.
            let validity_authorization = signed_7702(&validity_sponsor, 0, &validity_delegated)
                .with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(1_000_000),
                }]);
            admit(&pool, validity_authorization, subscribed).await.unwrap();
            admit(&pool, signed_1559(&validity_delegated, 0), subscribed).await.unwrap();
            assert_eip7702_error(
                admit(&pool, signed_1559(&validity_delegated, 1), subscribed).await,
                Eip7702PoolTransactionError::InflightTxLimitReached,
            );
        }
    }

    #[test]
    fn validity_insertion_past_a_gap_behind_a_blocked_head_reports_the_gap() {
        let mut nonce_pool =
            TwoDNoncePool::new(PriceBumpConfig::default()).with_mode(NoncePoolMode::Validity);
        let mut listeners = SidecarListeners::default();
        let signer = signer();
        let context = InsertContext {
            balance: Some((U256::from(1_000_000_000_000u64), nonce_pool.balance_epoch())),
            ..Default::default()
        };
        nonce_pool.set_block_info(ETHEREUM_BLOCK_GAS_LIMIT_30M, 2_000);
        nonce_pool
            .insert(valid_pool_transaction(validity_1559(&signer, 0, 1_000, 0)), context.clone())
            .unwrap();
        let mut all_events = listeners.subscribe_all();

        let outcome = nonce_pool
            .insert(valid_pool_transaction(validity_1559(&signer, 2, 3_000, 0)), context)
            .unwrap();
        listeners.on_inserted(&nonce_pool, &outcome);

        let queued: Vec<_> = std::iter::from_fn(|| all_events.next().now_or_never().flatten())
            .filter_map(|event| match event {
                FullTransactionEvent::Queued(_, reason) => Some(reason),
                _ => None,
            })
            .collect();
        assert_eq!(queued, vec![Some(QueuedReason::NonceGap)]);
    }

    /// The intra-block update also lowers the balance: an admission validated against the
    /// trailing canonical balance must not fund the lane past what the update left.
    #[tokio::test]
    async fn validity_admission_keeps_an_intra_block_balance_reduction() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        let address = sender.address();
        fund(&client, address);
        for nonce in 0..2 {
            pool.add_transaction(
                TransactionOrigin::External,
                validity_1559(&sender, nonce, 1_000, 0),
            )
            .await
            .unwrap();
        }

        // What the executed nonce 0 left covers exactly one more transaction.
        let one_transaction = U256::from(50_000u64 * 1_000);
        pool.update_accounts(vec![ChangedAccount { address, nonce: 1, balance: one_transaction }]);
        let next = validity_1559(&sender, 2, 1_000, 0);
        let next_hash = *next.hash();
        let added = pool.add_transaction(TransactionOrigin::External, next).await.unwrap();

        assert!(matches!(
            added.state,
            AddedTransactionState::Queued(QueuedReason::InsufficientBalance)
        ));
        assert_eq!(lane_nonces(&pool, address), (vec![1], vec![2]));
        assert!(!is_best(&pool, next_hash));
    }

    #[tokio::test]
    async fn protocol_authorization_index_tracks_each_pooled_authorization_once() {
        let (pool, client) = build_integration_pool();
        let [sponsor, authority] = std::array::from_fn(|_| signer());
        fund(&client, sponsor.address());
        let authorization = signed_7702(&sponsor, 0, &authority);
        let hash = *authorization.hash();
        let index_sizes = |pool: &IntegrationPool| {
            let index = pool.protocol_authorizations.lock();
            (index.authorizations_of(&authority.address()), index.len())
        };

        // Removal forgets the authorization, so re-admission records it afresh.
        for _ in 0..3 {
            pool.add_transaction(TransactionOrigin::External, authorization.clone()).await.unwrap();
            assert_eq!(index_sizes(&pool), (1, 1));
            pool.remove_transactions(vec![hash]);
            assert_eq!(index_sizes(&pool), (0, 0));
        }

        // Maintenance while the final admission stays pooled keeps exactly one reference.
        pool.add_transaction(TransactionOrigin::External, authorization).await.unwrap();
        canonical_update(&pool, 1, vec![], PoolUpdateKind::Commit);
        assert_eq!(index_sizes(&pool), (1, 1));
        assert!(pool.has_protocol_authorization(&authority.address()));

        pool.remove_transactions(vec![hash]);
        assert_eq!(index_sizes(&pool), (0, 0));
        assert!(!pool.has_protocol_authorization(&authority.address()));
    }

    /// A parked validity transaction discarded after a normal one was yielded must not drop the
    /// unrelated validity lane head the merge had already buffered.
    #[tokio::test]
    async fn discarding_a_parked_validity_transaction_keeps_a_buffered_lane_head() {
        let (pool, client) = build_integration_pool();
        let [parked_sender, normal, lane] = std::array::from_fn(|_| signer());
        for account in [&parked_sender, &normal, &lane] {
            fund(&client, account.address());
        }
        let parked = validity_1559(&parked_sender, 0, 3_000, 2_000);
        let parked_hash = *parked.hash();
        let normal_tx = signed_1559_with_fees(&normal, 0, 3_000, 1_500);
        let normal_hash = *normal_tx.hash();
        let head = validity_1559(&lane, 0, 3_000, 1_000);
        let successor = validity_1559(&lane, 1, 3_000, 900);
        let expected = vec![*head.hash(), *successor.hash()];
        for transaction in [parked, normal_tx, head, successor] {
            pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        }

        let mut best = pool.best_transactions_with_attributes_and_parking(
            BestTransactionsAttributes::new(0, None),
        );
        let first = best.next().expect("highest-tip validity transaction");
        assert_eq!(*first.hash(), parked_hash);
        best.park(&first);
        assert_eq!(best.next().map(|tx| *tx.hash()), Some(normal_hash));
        assert!(best.discard_parked(parked_hash, InvalidPoolTransactionError::Underpriced));

        // Each yielded transaction commits, releasing its lane successor.
        let rest: Vec<_> = std::iter::from_fn(|| {
            let transaction = best.next()?;
            best.mark_committed(&transaction);
            Some(*transaction.hash())
        })
        .collect();
        assert_eq!(rest, expected);
    }

    #[tokio::test]
    async fn protocol_authorization_index_stays_bounded_under_capacity_eviction() {
        let (pool, client) = build_integration_pool_with_config(PoolConfig {
            pending_limit: SubPoolLimit::new(1, usize::MAX),
            ..PoolConfig::default()
        });

        // Each better-paying authorization evicts the previous one from the full pool, and no
        // canonical update or authority lookup runs in between.
        let mut last = None;
        for tip in 1..=20u128 {
            let [sponsor, authority] = std::array::from_fn(|_| signer());
            fund(&client, sponsor.address());
            let transaction = signed_7702_with_tip(&sponsor, 0, &authority, tip);
            last = Some((*transaction.hash(), authority.address()));
            pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        }

        let (hash, authority) = last.unwrap();
        assert!(pool.protocol_pool.get(&hash).is_some());
        let index = pool.protocol_authorizations.lock();
        assert!(index.len() <= 3, "evicted authorizations accumulated: {}", index.len());
        assert!(index.backlog() <= 6, "reconciliation backlog grew: {}", index.backlog());
        assert_eq!(index.authorizations_of(&authority), 1);
    }

    #[tokio::test]
    async fn a_plain_replacement_forgets_the_authorization_it_replaced() {
        let (pool, client) = build_integration_pool();
        let [sponsor, authority] = std::array::from_fn(|_| signer());
        fund(&client, sponsor.address());
        let authorization = signed_7702(&sponsor, 0, &authority);
        pool.add_transaction(TransactionOrigin::External, authorization).await.unwrap();
        assert_eq!(pool.protocol_authorizations.lock().len(), 1);

        // A fee-bumped transaction without authorizations replaces it at the same nonce.
        pool.add_transaction(
            TransactionOrigin::External,
            signed_1559_with_fees(&sponsor, 0, 3_000, 2_000),
        )
        .await
        .unwrap();

        assert!(pool.protocol_authorizations.lock().is_empty());
        assert!(!pool.has_protocol_authorization(&authority.address()));
    }

    /// The `txpool.validity.pooled` gauge value in `snapshot`, if recorded.
    fn validity_pooled_gauge(
        snapshot: Vec<(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )>,
    ) -> Option<f64> {
        snapshot.into_iter().find_map(|(key, _, _, value)| match value {
            metrics_util::debugging::DebugValue::Gauge(value)
                if key.key().name() == "txpool.validity.pooled" =>
            {
                Some(value.into_inner())
            }
            _ => None,
        })
    }

    /// A replacement evicted on insertion still removed its incumbent from the validity store,
    /// so the gauges follow it.
    #[tokio::test]
    async fn replacement_discarded_on_insert_refreshes_store_gauges() {
        let (pool, client) = build_integration_pool();
        let [sender, authority] = std::array::from_fn(|_| signer());
        fund(&client, sender.address());
        let incumbent = validity_1559(&sender, 0, 500, 0);
        // Room for the incumbent only: the larger replacement exceeds the byte cap.
        let pool = pool.with_validity_pool_config(ValidityPoolConfig {
            max_count: 16,
            max_size_bytes: incumbent.size(),
            max_account_slots: 16,
        });
        pool.add_transaction(TransactionOrigin::External, incumbent).await.unwrap();
        let replacement =
            signed_7702_with_tip(&sender, 0, &authority, 0).with_validity_predicates(vec![
                ValidityPredicate::BlockNumber {
                    op: ValidityOperator::LessThanOrEqual,
                    value: U256::from(1_000_000),
                },
            ]);
        let epoch = pool.validity_pool.read().balance_epoch();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::External, replacement).await;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let result = metrics::with_local_recorder(&recorder, || {
            pool.add_validated_custom_transaction(validated, TransactionOrigin::External, epoch)
        });

        let error = result.expect_err("the replacement exceeds the byte cap");
        assert!(matches!(error.kind, PoolErrorKind::DiscardedOnInsert));
        assert_eq!(pool.validity_pool.read().len(), 0);
        assert_eq!(validity_pooled_gauge(snapshotter.snapshot().into_vec()), Some(0.0));
    }

    /// A replacement validated before a feed gap is stale: rejecting it must leave the incumbent
    /// pooled with its authorization still limiting the authority.
    #[tokio::test]
    async fn stale_validity_replacement_keeps_the_incumbent_authorization() {
        let (pool, client) = build_zenith_integration_pool();
        let [sponsor, authority] = std::array::from_fn(|_| signer());
        fund(&client, sponsor.address());
        fund(&client, authority.address());
        let incumbent = signed_7702(&sponsor, 0, &authority).with_validity_predicates(vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1_000_000),
            },
        ]);
        let incumbent_hash = *incumbent.hash();
        pool.add_transaction(TransactionOrigin::External, incumbent).await.unwrap();
        let replacement = signed_8130(&sponsor, U256::ZERO, 0, 0, 2_000, 1_000_000)
            .with_validity_predicates(vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1_000_000),
            }]);
        let epoch = pool.validity_pool.read().balance_epoch();
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::External, replacement).await;

        pool.invalidate_all_tracked_transactions(InvalidationCause::FeedGap);
        let result =
            pool.add_validated_custom_transaction(validated, TransactionOrigin::External, epoch);

        assert!(result.is_err(), "a stale classification must be rejected");
        assert!(pool.validity_pool.read().contains(&incumbent_hash));
        pool.add_transaction(TransactionOrigin::External, signed_1559(&authority, 0))
            .await
            .unwrap();
        assert_eip7702_error(
            pool.add_transaction(TransactionOrigin::External, signed_1559(&authority, 1)).await,
            Eip7702PoolTransactionError::InflightTxLimitReached,
        );
    }

    /// A standard validity transaction holds no guard capacity, so replacing it with an
    /// EIP-8130 transaction is charged exactly like a fresh submission.
    #[tokio::test]
    async fn replacing_an_unguarded_validity_transaction_is_charged_like_a_fresh_one() {
        let (pool, client) = build_zenith_integration_pool();
        let pool = pool.with_guard_limits(GuardLimits { signature_limit: 0, ..Default::default() });
        let [sender, fresh_sender] = std::array::from_fn(|_| signer());
        fund(&client, sender.address());
        fund(&client, fresh_sender.address());
        let predicates = || {
            vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1_000_000),
            }]
        };
        let incumbent = validity_1559(&sender, 0, 1_000, 0);
        let incumbent_hash = *incumbent.hash();
        pool.add_transaction(TransactionOrigin::External, incumbent).await.unwrap();

        let fresh = pool
            .add_transaction(
                TransactionOrigin::External,
                signed_8130(&fresh_sender, U256::ZERO, 0, 0, 2_000, 1_000_000)
                    .with_validity_predicates(predicates()),
            )
            .await
            .expect_err("the exhausted guard must reject a fresh submission");
        let replacement = pool
            .add_transaction(
                TransactionOrigin::External,
                signed_8130(&sender, U256::ZERO, 0, 0, 2_000, 1_000_000)
                    .with_validity_predicates(predicates()),
            )
            .await
            .expect_err("the exhausted guard must reject the replacement");

        assert_eq!(replacement.kind.to_string(), fresh.kind.to_string());
        assert!(pool.validity_pool.read().contains(&incumbent_hash));
        assert!(pool.guard.read().tracked_hashes().is_empty());
        assert!(pool.validity_guard.read().tracked_hashes().is_empty());
    }

    /// A trivially satisfied block-number validity predicate.
    fn any_block_predicate() -> Vec<ValidityPredicate> {
        vec![ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(1_000_000),
        }]
    }

    /// A validity EIP-8130 transaction consumed by an external account-nonce advance is an
    /// EIP-8130 state-diff invalidation, counted once.
    #[tokio::test]
    async fn externally_consumed_validity_8130_counts_as_a_state_diff_invalidation() {
        let (pool, client) = build_integration_pool();
        let sender = signer();
        fund(&client, sender.address());
        let guarded = self_paid_eoa_8130(&sender, U256::ZERO, 0, 0, 1_000)
            .with_validity_predicates(any_block_predicate());
        let guarded_hash = *guarded.hash();
        pool.add_transaction(TransactionOrigin::External, guarded).await.unwrap();
        assert!(pool.validity_guard.read().contains(&guarded_hash));

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let removed = metrics::with_local_recorder(&recorder, || {
            pool.apply_state_diff(&[AccountStateDiff {
                address: sender.address(),
                nonce: Some(1),
                ..Default::default()
            }])
        });

        assert!(removed.iter().any(|transaction| *transaction.hash() == guarded_hash));
        let invalidated = snapshotter.snapshot().into_vec().into_iter().find_map(
            |(key, _, _, value)| match value {
                metrics_util::debugging::DebugValue::Counter(value)
                    if key.key().name() == "txpool.guard.invalidated"
                        && key.key().labels().any(|label| {
                            label.key() == "cause" && label.value() == "state_diff"
                        }) =>
                {
                    Some(value)
                }
                _ => None,
            },
        );
        assert_eq!(invalidated, Some(1));
    }

    /// A queued successor does not strip the coinbase-tip rank of the cumulatively funded
    /// validity account-lane head ahead of it.
    #[tokio::test]
    async fn a_funded_validity_head_keeps_its_tip_rank_behind_a_queued_successor() {
        let (pool, client) = build_integration_pool();
        let [sender, other] = std::array::from_fn(|_| signer());
        client
            .add_account(sender.address(), ExtendedAccount::new(0, U256::from(5) * tipping_fees()));
        fund(&client, other.address());
        // Each transaction costs three times the tipping fees: only nonce 0 is funded.
        let tip = U256::from(2) * tipping_fees();
        let head =
            tipping_eoa_8130(&sender, None, 0, tip).with_validity_predicates(any_block_predicate());
        let head_hash = *head.hash();
        let successor =
            tipping_eoa_8130(&sender, None, 1, tip).with_validity_predicates(any_block_predicate());
        let successor_hash = *successor.hash();
        pool.add_transaction(TransactionOrigin::External, head).await.unwrap();
        pool.add_transaction(TransactionOrigin::External, successor).await.unwrap();
        pool.add_transaction(
            TransactionOrigin::External,
            signed_1559_with_fees(&other, 0, 1_000, 1),
        )
        .await
        .unwrap();

        let best = best_hashes(&pool);
        assert_eq!(best.first(), Some(&head_hash), "the funded tip outranks priority fee 1");
        assert!(!best.contains(&successor_hash));
    }
}
