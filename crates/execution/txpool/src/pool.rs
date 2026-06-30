//! Base transaction-pool wrapper that combines the protocol pool with a 2D nonce sidecar.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B128, B256, TxHash, U256, map::AddressSet};
use futures::StreamExt;
use parking_lot::RwLock;
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStore, BlobStoreError, BlockInfo, FullTransactionEvent,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool, PoolResult, PoolSize,
    PoolTransaction, PropagatedTransactions, SubPool, TransactionEvents, TransactionListenerKind,
    TransactionOrigin, TransactionPool, TransactionPoolExt, TransactionValidationOutcome,
    TransactionValidationTaskExecutor, TransactionValidator, ValidPoolTransaction,
    pool::{AddedTransactionState, TransactionEvent},
};
use tokio::{spawn, sync::mpsc};

use crate::{
    Admission, BasePooledTx, BaseTransactionValidator, GuardLimits, InvalidationKey, LimitRejection,
    MempoolGuard,
    best::MergeBestTransactions,
    two_d_nonce_pool::{InsertOutcome, TwoDNoncePool},
};

const SIDE_CAR_EVENT_CHANNEL_SIZE: usize = 1024;

/// A per-account state delta from a committed block, used to drive sidecar
/// mempool invalidation.
///
/// Produced by the canonical-state feeder from the block's `BundleState`: each
/// changed account contributes its balance/nonce change and the set of its
/// contract storage slots that changed. Fed to
/// [`BaseTransactionPool::apply_state_diff`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountStateDiff {
    /// The account whose state changed (also the contract address for any
    /// [`Self::changed_slots`]).
    pub address: Address,
    /// The new balance, when it changed this block. Drives threshold
    /// re-evaluation of payer reservations.
    pub balance: Option<U256>,
    /// Whether the account's protocol (EOA) nonce changed this block. An
    /// exact-match drop for transactions watching it.
    pub nonce_changed: bool,
    /// Storage slots in this account's contract whose value changed this block
    /// (e.g. actor-config, account-lock, or channel-nonce slots). Each is an
    /// exact-match drop for transactions watching it.
    pub changed_slots: Vec<B256>,
}

impl AccountStateDiff {
    /// Creates an empty diff for `address`.
    #[must_use]
    pub fn new(address: Address) -> Self {
        Self { address, ..Default::default() }
    }

    /// Appends this account's exact-match invalidation keys (changed protocol
    /// nonce and changed storage slots) to `out`.
    fn push_exact_keys(&self, out: &mut Vec<InvalidationKey>) {
        if self.nonce_changed {
            out.push(InvalidationKey::ProtocolNonce(self.address));
        }
        for slot in &self.changed_slots {
            out.push(InvalidationKey::Slot { address: self.address, slot: *slot });
        }
    }
}

/// Builds the guard admission for a validated EIP-8130 transaction (sidecar or
/// reth-resident) from the watch set and limit classification derived during
/// validation. Returns `None` for non-EIP-8130 transactions — standard
/// 1559/legacy transactions also carry a `watch_set`/`limit_class`, but reth's
/// own maintenance covers their (sender balance + nonce) invalidation and they
/// must not be charged the 8130 admission caps, so they are neither guard-gated
/// nor guard-tracked here.
fn admission_for<T>(transaction: &T) -> Option<Admission>
where
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
{
    transaction.as_eip8130()?;
    let watch_set = transaction.watch_set().cloned()?;
    let class = *transaction.limit_class()?;
    Some(Admission {
        hash: *transaction.hash(),
        sender: class.sender,
        payer: class.payer,
        sender_locked: class.sender_locked,
        payer_trusted: class.payer_trusted,
        payer_balance: class.payer_balance,
        max_cost: class.max_cost,
        priority: transaction.priority_fee_or_price(),
        watch_set,
    })
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
    eip8130_replays: Arc<RwLock<HashMap<B256, TxHash>>>,
    listeners: Arc<RwLock<SidecarListeners<T>>>,
    /// State-keyed admission and invalidation ledger covering both the 2D-nonce
    /// sidecar and every reth-resident EIP-8130 transaction (sponsored `nonce_key
    /// == 0` and nonce-free `NONCE_KEY_MAX`). It is the single authority for the
    /// dual sender/payer admission limits and for state-keyed invalidation
    /// (exact-match slot/nonce drops, payer-balance thresholds, expiry buckets),
    /// so the two pools can never make divergent eviction decisions for the same
    /// transaction. Dropped hashes are partitioned back to whichever pool holds
    /// them. Count drift from reth-internal evictions/replacements is bounded by
    /// the per-block reconcile in [`Self::reconcile_guard`].
    guard: Arc<RwLock<MempoolGuard>>,
    /// Highest expiry bucket already fired (`None` until the first canonical
    /// update). Each block fires only the newly-due bucket(s) — typically one —
    /// in `(last + 1 ..= horizon)`, so expiry eviction never rescans the index.
    last_fired_expiry_bucket: Arc<RwLock<Option<u64>>>,
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
            eip8130_replays: Arc::clone(&self.eip8130_replays),
            listeners: Arc::clone(&self.listeners),
            guard: Arc::clone(&self.guard),
            last_fired_expiry_bucket: Arc::clone(&self.last_fired_expiry_bucket),
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
            eip8130_replays: Arc::new(RwLock::new(HashMap::new())),
            listeners: Arc::new(RwLock::new(SidecarListeners::default())),
            guard: Arc::new(RwLock::new(MempoolGuard::new(GuardLimits::default()))),
            last_fired_expiry_bucket: Arc::new(RwLock::new(None)),
        }
    }

    /// Sets the per-account admission caps enforced by the sidecar guard.
    ///
    /// Must be called at construction, before any transaction is admitted, so the
    /// guard starts empty under the configured limits.
    #[must_use]
    pub fn with_guard_limits(self, limits: GuardLimits) -> Self {
        *self.guard.write() = MempoolGuard::new(limits);
        self
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
        transaction.eip8130_nonce_channel_key().is_some()
    }

    /// Maps a guard admission-limit rejection to a pool error returned to the
    /// submitter.
    fn limit_rejection_error(
        hash: TxHash,
        rejection: LimitRejection,
    ) -> reth_transaction_pool::error::PoolError {
        let reason = match rejection {
            LimitRejection::SenderLimit => "sender EIP-8130 mempool limit reached",
            LimitRejection::PayerLimit => "payer EIP-8130 mempool limit reached",
            LimitRejection::PayerBalance => {
                "payer balance cannot fund another sponsored EIP-8130 transaction"
            }
        };
        reth_transaction_pool::error::PoolError::other(hash, reason)
    }

    /// Applies a committed block's state diff to the sidecar invalidation guard
    /// and removes every newly-invalid channelized transaction from the sidecar.
    ///
    /// Exact-match surfaces (changed protocol nonces, actor-config / account-lock
    /// / channel-nonce storage slots) drop their watchers unconditionally;
    /// changed payer balances re-evaluate balance-bounded reservations
    /// (threshold). Returns the removed transactions (already broadcast as
    /// discarded to listeners).
    ///
    /// This is the M6 feeder entry point: a canonical-state task extracts the
    /// per-account `BundleState` deltas of each committed block and calls this
    /// once per block, invalidating ahead of the builder via the O(watchers)
    /// reverse index rather than an O(pool) rescan.
    ///
    /// Lock order: the guard is taken alone (computing the drop set), released,
    /// then the nonce pool and listeners — matching `update_accounts` so the
    /// global order stays acyclic.
    pub fn apply_state_diff(&self, diffs: &[AccountStateDiff]) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut exact = Vec::new();
        for diff in diffs {
            diff.push_exact_keys(&mut exact);
        }

        // Single-authority invalidation through the guard, which tracks both the
        // 2D-nonce sidecar and every reth-resident EIP-8130 transaction. Exact
        // surfaces drop unconditionally; changed balances re-evaluate payer
        // reservations (threshold/aggregate). The dropped set is then routed back
        // to whichever pool holds each hash.
        let dropped = {
            let mut guard = self.guard.write();
            let mut dropped = guard.invalidate_exact(exact);
            for diff in diffs {
                if let Some(balance) = diff.balance {
                    dropped.extend(guard.on_balance_changed(diff.address, balance));
                }
            }
            dropped
        };
        self.remove_dropped_across_pools(dropped)
    }

    /// Removes a guard-dropped set of hashes from whichever pool holds each one,
    /// broadcasting sidecar discards and untracking nonce-free replay ids. The
    /// guard bookkeeping was already released when the hashes were produced
    /// (`invalidate_exact` / `on_balance_changed` release as they drop), so this
    /// only touches the pools.
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
            let protocol_removed = self.protocol_pool.remove_transactions(protocol_hashes);
            self.untrack_eip8130_replays(&protocol_removed);
            protocol_removed
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

    /// Fires the expiry bucket(s) that have come due as of `now`, dropping every
    /// transaction whose effective expiry falls in them from both the sidecar
    /// (guard-owned) and the protocol pool.
    ///
    /// Uses a one-block lookahead (`now + EXPIRY_BUCKET_SECS`) so transactions
    /// that cannot survive into the next block are evicted while the current
    /// block is built. Only the newly-due bucket(s) — typically one — are fired,
    /// tracked by `last_fired_expiry_bucket`, so this never rescans the index.
    fn expire_due_buckets(&self, now: u64) {
        let horizon_bucket = now.saturating_add(InvalidationKey::EXPIRY_BUCKET_SECS)
            / InvalidationKey::EXPIRY_BUCKET_SECS;
        let keys: Vec<InvalidationKey> = {
            let mut last = self.last_fired_expiry_bucket.write();
            let start = last.map_or(horizon_bucket, |prev| prev.saturating_add(1));
            *last = Some(last.map_or(horizon_bucket, |prev| prev.max(horizon_bucket)));
            if start > horizon_bucket {
                return;
            }
            (start..=horizon_bucket).map(InvalidationKey::ExpiryBucket).collect()
        };

        // Both sidecar and reth-resident members are tracked by the guard, so a
        // single invalidate_exact yields every expired hash; route each back to
        // the pool that holds it.
        let dropped = self.guard.write().invalidate_exact(keys);
        self.remove_dropped_across_pools(dropped);
    }

    /// Releases the guard bookkeeping for a batch of removed sidecar
    /// transactions. Acquired on its own (never nested under the nonce pool or
    /// listener locks) so the lock order stays acyclic.
    fn release_from_guard(&self, removed: &[Arc<ValidPoolTransaction<T>>]) {
        if removed.is_empty() {
            return;
        }
        let mut guard = self.guard.write();
        for transaction in removed {
            guard.release(transaction.hash());
        }
    }

    /// Best-effort replay-id dedup lookup for the mempool admission path.
    ///
    /// This check and the subsequent [`Self::track_eip8130_replay_id`] insert are
    /// deliberately **not** atomic: the index lock is released between them (and
    /// between this check and the actual pool insert). Two concurrent admissions
    /// of the same `replay_id` can therefore both pass and enter the
    /// pool. That is acceptable because the index is only a mempool optimization,
    /// not a consensus control: identical transactions collapse by tx-hash in the
    /// underlying pool, and any surviving nonce-free duplicate is rejected at
    /// execution by the enshrined replay buffer. Holding the index lock across the
    /// validate+insert would also serialize admission and reintroduce cross-lock
    /// nesting, so the looser guarantee is intentional. A stale entry whose target
    /// is no longer pooled is opportunistically evicted here.
    fn eip8130_replay_already_seen(&self, transaction: &T) -> Option<TxHash> {
        // `replay_id` already commits to the resolved sender, so it alone keys the
        // index (matching the enshrined replay buffer, which keys by `replay_id`).
        let replay_id = transaction.eip8130_replay_id()?;
        let hash = self.eip8130_replays.read().get(&replay_id).copied()?;
        // Only nonce-free transactions have replay IDs, and those are only ever
        // admitted to the protocol pool; channelized transactions live in
        // `nonce_pool` and never carry a replay ID. Guard that routing invariant
        // so the protocol-pool-only liveness check below stays sound if routing
        // or replay-id derivation ever evolve independently.
        debug_assert!(
            !self.nonce_pool.read().contains(&hash),
            "eip8130 replay index points at a sidecar-pool transaction",
        );
        if self.protocol_pool.get(&hash).is_some() {
            return Some(hash);
        }
        self.eip8130_replays.write().remove(&replay_id);
        None
    }

    fn track_eip8130_replay_id(&self, replay_id: B256, hash: TxHash) {
        {
            let mut index = self.eip8130_replays.write();
            index.insert(replay_id, hash);
        }
        self.reconcile_eip8130_replays_if_needed();
    }

    fn reconcile_eip8130_replays_if_needed(&self) {
        let pool_size = self.pool_size().total;
        // Fast path: bail while within bound, holding only the index read lock.
        if self.eip8130_replays.read().len() <= pool_size {
            return;
        }
        // Snapshot and rebuild the index from the live pool *without* holding the
        // `eip8130_replays` lock. `pooled_transactions()` takes `nonce_pool.read()`,
        // so acquiring the index write lock first (as the naive `write(); rebuild()`
        // would) establishes an `eip8130_replays -> nonce_pool` lock order. Building
        // the replacement outside the lock keeps the two locks strictly disjoint,
        // avoiding a lock-order inversion with any path that touches the index after
        // the nonce pool. Entries added between this snapshot and the write below are
        // best-effort only (the index is a dedup optimization, not consensus state).
        let mut rebuilt = HashMap::new();
        for transaction in self.pooled_transactions() {
            if let Some(replay_id) = transaction.transaction.eip8130_replay_id() {
                rebuilt.insert(replay_id, *transaction.hash());
            }
        }
        let mut index = self.eip8130_replays.write();
        // Re-check under the write lock: only overwrite while still oversized.
        if index.len() > pool_size {
            *index = rebuilt;
        }
    }

    fn untrack_eip8130_replays(&self, transactions: &[Arc<ValidPoolTransaction<T>>]) {
        let mut index = self.eip8130_replays.write();
        for transaction in transactions {
            if let Some(replay_id) = transaction.transaction.eip8130_replay_id() {
                index.remove(&replay_id);
            }
        }
    }

    fn untrack_eip8130_hashes(&self, hashes: &[TxHash]) {
        let hashes = hashes.iter().collect::<HashSet<_>>();
        let mut index = self.eip8130_replays.write();
        index.retain(|_, indexed_hash| !hashes.contains(indexed_hash));
    }

    /// Gates a freshly-added reth-resident EIP-8130 transaction (sponsored
    /// `nonce_key == 0` or nonce-free) through the guard's dual sender/payer
    /// admission limits and, on success, registers its watch set so committed
    /// actor-config slot changes, payer-balance drops and expiry buckets
    /// invalidate it ahead of the builder — surfaces reth's own maintenance does
    /// not track for these transactions.
    ///
    /// The transaction was already validated and inserted into the protocol pool,
    /// so a limit rejection removes it again (leaving no trace) and returns the
    /// limit error to the submitter. Non-EIP-8130 transactions are not tracked.
    ///
    /// A fee-bump that *replaces* an existing tracked transaction while its
    /// sender/payer dimension is already at the cap can be rejected here (and the
    /// replacement removed) even though reth accepted the swap; the stale record
    /// of the replaced transaction is reclaimed by the per-block
    /// [`Self::reconcile_guard`], after which a resubmission succeeds. This is the
    /// accepted over-rejection direction of the add-gate + reconcile model.
    fn gate_protocol_admission(
        &self,
        hash: TxHash,
    ) -> Result<(), reth_transaction_pool::error::PoolError> {
        let Some(validated) = self.protocol_pool.get(&hash) else {
            return Ok(());
        };
        let Some(admission) = admission_for(&validated.transaction) else {
            return Ok(());
        };
        if let Err(rejection) = self.guard.write().try_admit(admission) {
            let removed = self.protocol_pool.remove_transactions(vec![hash]);
            self.untrack_eip8130_replays(&removed);
            return Err(Self::limit_rejection_error(hash, rejection));
        }
        Ok(())
    }

    /// Releases the guard bookkeeping for a batch of removed reth-resident
    /// transactions (the protocol-pool counterpart of [`Self::release_from_guard`]).
    /// Non-tracked (non-EIP-8130) hashes are a no-op.
    fn release_protocol_from_guard(&self, removed: &[Arc<ValidPoolTransaction<T>>]) {
        if removed.is_empty() {
            return;
        }
        let mut guard = self.guard.write();
        for transaction in removed {
            guard.release(transaction.hash());
        }
    }

    /// Reclaims guard bookkeeping for any tracked transaction no longer resident
    /// in either pool. reth can evict or replace protocol-pool transactions
    /// without routing through our removal paths, which leaves stale admission
    /// reservations; this per-block sweep releases them so counts cannot drift
    /// upward over time. It is O(tracked) and only ever *frees* capacity, so it
    /// never rejects a valid transaction.
    fn reconcile_guard(&self) {
        let tracked = self.guard.read().tracked_hashes();
        if tracked.is_empty() {
            return;
        }
        let nonce_pool = self.nonce_pool.read();
        let stale: Vec<TxHash> = tracked
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
                let admission = admission_for(&validated.transaction);
                let outcome = nonce_pool.insert_validated(validated, state_nonce)?;
                // Apply admission limits while still holding the nonce-pool lock
                // (which serializes every guard-touching pool path), then drop the
                // guard before notifying listeners to keep the lock order acyclic.
                {
                    let mut guard = self.guard.write();
                    match (&outcome.replaced, admission) {
                        // A replacement (fee-bump of an existing channel nonce) is
                        // net-neutral on the count dimensions: release the old and
                        // force the new in, never rejecting.
                        (Some(replaced), admission) => {
                            guard.release(replaced.hash());
                            if let Some(admission) = admission {
                                guard.insert_forced(admission);
                            }
                        }
                        // A genuinely new transaction is gated by the limits.
                        (None, Some(admission)) => {
                            if let Err(rejection) = guard.try_admit(admission) {
                                drop(guard);
                                // Roll the insert back before any listener observes
                                // it, so a limit rejection leaves no trace.
                                let hash = outcome.outcome.hash;
                                nonce_pool.remove_transactions(&[hash]);
                                return Err(Self::limit_rejection_error(hash, rejection));
                            }
                        }
                        (None, None) => {}
                    }
                }
                listeners.on_inserted(&nonce_pool, &outcome);
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
        let sender_id = nonce_pool
            .transactions_by_sender(transaction.sender())
            .first()
            .map(|transaction| transaction.sender_id())
            .unwrap_or_else(|| nonce_pool.sender_id_or_create(transaction.sender()));
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

impl<Client, S, Evm, T, O> crate::StateDiffInvalidation
    for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: Send + Sync + 'static,
    Evm: Send + Sync + 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn invalidate_from_state_diff(&self, diffs: &[AccountStateDiff]) -> usize {
        self.apply_state_diff(diffs).len()
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
        if self.eip8130_replay_already_seen(&transaction).is_some() {
            // TODO: Replace the indexed transaction when the new priority fee
            // satisfies the pool's configured price bump.
            return Err(reth_transaction_pool::error::PoolError::new(
                *transaction.hash(),
                reth_transaction_pool::error::PoolErrorKind::AlreadyImported,
            ));
        }
        if !self.is_sidecar_transaction(&transaction) {
            let replay_id = transaction.eip8130_replay_id();
            let hash = *transaction.hash();
            let events =
                self.protocol_pool.add_transaction_and_subscribe(origin, transaction).await?;
            self.gate_protocol_admission(hash)?;
            if let Some(replay_id) = replay_id {
                self.track_eip8130_replay_id(replay_id, hash);
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
        if self.eip8130_replay_already_seen(&transaction).is_some() {
            // TODO: Replace the indexed transaction when the new priority fee
            // satisfies the pool's configured price bump.
            return Err(reth_transaction_pool::error::PoolError::new(
                *transaction.hash(),
                reth_transaction_pool::error::PoolErrorKind::AlreadyImported,
            ));
        }
        if self.is_sidecar_transaction(&transaction) {
            self.add_sidecar_transaction(origin, transaction).await
        } else {
            let replay_id = transaction.eip8130_replay_id();
            let hash = *transaction.hash();
            let outcome = self.protocol_pool.add_transaction(origin, transaction).await?;
            self.gate_protocol_admission(hash)?;
            if let Some(replay_id) = replay_id {
                self.track_eip8130_replay_id(replay_id, hash);
            }
            Ok(outcome)
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
            if let Some(transaction) = self.protocol_pool.get(hash) {
                let Some((pooled, encoded_length)) = pooled_element(&transaction) else {
                    continue;
                };
                current_size += encoded_length;
                if limit.exceeds(current_size) {
                    break;
                }
                out.push(pooled);
                continue;
            }

            let Some(transaction) = self.nonce_pool.read().get(hash) else {
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
        // Channelized nonce sequences live in a separate namespace from account nonces, so this
        // sender+nonce lookup intentionally remains protocol-only.
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
        self.untrack_eip8130_replays(&removed);
        self.release_protocol_from_guard(&removed);
        let sidecar_removed = self.nonce_pool.write().remove_transactions(&sidecar_hashes);
        if !sidecar_removed.is_empty() {
            self.untrack_eip8130_replays(&sidecar_removed);
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
        self.untrack_eip8130_replays(&removed);
        self.release_protocol_from_guard(&removed);
        let sidecar_removed =
            self.nonce_pool.write().remove_transactions_and_descendants(&sidecar_hashes);
        if !sidecar_removed.is_empty() {
            self.untrack_eip8130_replays(&sidecar_removed);
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
        self.untrack_eip8130_replays(&removed);
        self.release_protocol_from_guard(&removed);
        let sidecar_removed = self.nonce_pool.write().remove_transactions_by_sender(sender);
        if !sidecar_removed.is_empty() {
            self.untrack_eip8130_replays(&sidecar_removed);
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
        self.untrack_eip8130_replays(&removed);
        self.release_protocol_from_guard(&removed);
        let pruned = self.nonce_pool.write().prune_mined(&sidecar_hashes);
        self.untrack_eip8130_replays(&pruned.removed);
        self.release_from_guard(&pruned.removed);
        removed.extend(pruned.removed);
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

    fn blob_store(&self) -> Box<dyn BlobStore> {
        Box::new(self.protocol_pool.blob_store().clone())
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
        let mined_transactions = update.mined_transactions.clone();
        self.untrack_eip8130_hashes(&mined_transactions);
        self.protocol_pool.on_canonical_state_change(update);
        {
            let mut nonce_pool = self.nonce_pool.write();
            let pruned = nonce_pool.prune_mined(&mined_transactions);
            let mut listeners = self.listeners.write();
            if !pruned.removed.is_empty() {
                listeners.on_mined(&pruned.removed, block_hash);
            }
            // Lock order: nonce_pool -> listeners -> guard (acquired last).
            // Release the guard bookkeeping for every mined transaction: the
            // sidecar set precisely (`pruned.removed`) and the reth-resident set
            // by hash (`mined_transactions`); releases are idempotent and no-op
            // for untracked (non-EIP-8130) hashes.
            let mut guard = self.guard.write();
            for transaction in &pruned.removed {
                guard.release(transaction.hash());
            }
            for hash in &mined_transactions {
                guard.release(hash);
            }
        }
        // After the mined-state locks are released, fire any expiry buckets that
        // have come due (one-block lookahead) to evict transactions that cannot
        // survive into the next block.
        self.expire_due_buckets(now);
        // Reclaim any guard reservations whose transaction left a pool without
        // routing through our removal paths (reth-internal evictions, fee-bump
        // replacements), bounding admission-count drift to a single block.
        self.reconcile_guard();
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        // Drive sidecar balance invalidation through the state-keyed guard: each
        // changed account is an O(1) reverse-index lookup keyed on the *payer*
        // (correct for sponsored EIP-8130 transactions), replacing the previous
        // O(pool) sender-keyed affordability scan. The guard lock is taken on its
        // own and dropped before touching the nonce pool to keep the order
        // acyclic.
        // The guard is keyed on the *payer* (correct for sponsored EIP-8130
        // transactions, where the payer — not the sender — funds the gas), so a
        // single balance sweep covers both the sidecar and reth-resident members
        // that reth's own sender-keyed update_accounts would miss. Dropped hashes
        // are routed back to whichever pool holds them.
        let dropped = {
            let mut guard = self.guard.write();
            let mut dropped = Vec::new();
            for account in &accounts {
                dropped.extend(guard.on_balance_changed(account.address, account.balance));
            }
            dropped
        };
        self.remove_dropped_across_pools(dropped);

        self.protocol_pool.update_accounts(accounts)
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

        for promoted in &outcome.promoted {
            self.broadcast_pending_transaction(promoted);
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
            let propagate_event = event.clone();
            self.new_propagate
                .retain(|listener| listener.try_send(propagate_event.clone()).is_ok());
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
    let encoded_length = transaction.encoded_length();
    transaction
        .transaction
        .clone()
        .try_into_pooled()
        .ok()
        .map(|recovered| recovered.into_parts().0)
        .map(|pooled| (pooled, encoded_length))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_primitives::{Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::TxKind;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives, BaseTxEnvelope,
        Eip8130Constants, Eip8130Signed, TxEip8130,
    };
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_evm::BaseEvmConfig;
    use futures::StreamExt;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_tasks::Runtime;
    use reth_transaction_pool::{
        Pool, PoolConfig, PriceBumpConfig, TransactionOrigin, blobstore::InMemoryBlobStore,
        identifier::TransactionId, validate::EthTransactionValidatorBuilder,
    };

    use super::*;
    use crate::{BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, LimitClass, WatchSet};

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
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            expiry: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas,
            gas_limit: 50_000,
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

    #[test]
    fn account_state_diff_emits_exact_keys_for_nonce_and_slots() {
        let address = Address::repeat_byte(7);
        let slot_a = B256::repeat_byte(1);
        let slot_b = B256::repeat_byte(2);

        let diff = AccountStateDiff {
            address,
            balance: Some(U256::from(5u64)),
            nonce_changed: true,
            changed_slots: vec![slot_a, slot_b],
        };

        let mut keys = Vec::new();
        diff.push_exact_keys(&mut keys);

        // Balance is a threshold surface, so it is NOT an exact-match key here;
        // the nonce change and both storage slots are.
        assert_eq!(
            keys,
            vec![
                InvalidationKey::ProtocolNonce(address),
                InvalidationKey::Slot { address, slot: slot_a },
                InvalidationKey::Slot { address, slot: slot_b },
            ]
        );
    }

    #[test]
    fn account_state_diff_without_changes_emits_no_exact_keys() {
        let diff = AccountStateDiff::new(Address::repeat_byte(3));
        let mut keys = Vec::new();
        diff.push_exact_keys(&mut keys);
        assert!(keys.is_empty());
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

    /// Builds a reth-resident (non-channelized) EIP-8130 transaction — a
    /// sponsored `nonce_key == 0` or nonce-free (`NONCE_KEY_MAX`) — and attaches
    /// the `LimitClass`/`WatchSet` the validator would derive, so the admission
    /// it produces via [`admission_for`] is exactly what the add path gates with.
    /// A fresh random signer per call yields a distinct hash.
    fn classified_eip8130(
        nonce_key: U256,
        nonce_sequence: u64,
        sender: Address,
        payer: Address,
        payer_trusted: bool,
        expiry: Option<u64>,
    ) -> BasePooledTransaction {
        let signer = signer();
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            expiry: expiry.unwrap_or(0),
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        let transaction =
            BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()));

        let mut watch_set = WatchSet::new().watch(InvalidationKey::Balance(payer));
        if let Some(expiry) = expiry {
            watch_set = watch_set.watch(InvalidationKey::expiry_bucket(expiry));
        }
        transaction.set_watch_set(watch_set);
        transaction.set_limit_class(LimitClass {
            sender,
            payer,
            sender_locked: false,
            payer_trusted,
            payer_balance: U256::from(1_000_000u64),
            max_cost: U256::from(1_000u64),
        });
        transaction
    }

    #[test]
    fn nonce_zero_admission_is_gated_by_sender_limit() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = Address::repeat_byte(0x11);
        let cap = GuardLimits::default().default_sender;

        // Self-paying `nonce_key == 0` transactions each charge the sender
        // dimension; the cap-th+1 is rejected exactly like a channelized tx.
        for seq in 0..u64::from(cap) {
            let tx = classified_eip8130(U256::ZERO, seq, sender, sender, false, None);
            let admission = admission_for(&tx).expect("classified 8130 tx yields an admission");
            assert!(guard.try_admit(admission).is_ok());
        }
        let over = classified_eip8130(U256::ZERO, u64::from(cap), sender, sender, false, None);
        let admission = admission_for(&over).unwrap();
        assert_eq!(guard.try_admit(admission), Err(LimitRejection::SenderLimit));
    }

    #[test]
    fn nonce_zero_sponsored_admission_is_gated_by_payer_limit() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = Address::repeat_byte(0x22);
        let cap = GuardLimits::default().default_payer;

        // Distinct senders sponsored by one (count-limited) payer: the sender
        // dimension never binds, so the payer dimension is what gates a sponsored
        // `nonce_key == 0` transaction — the surface reth's sender-keyed limits
        // would miss.
        for i in 0..u64::from(cap) {
            let sender = Address::repeat_byte(0x30 + i as u8);
            let tx = classified_eip8130(U256::ZERO, i, sender, payer, false, None);
            assert!(guard.try_admit(admission_for(&tx).unwrap()).is_ok());
        }
        let sender = Address::repeat_byte(0x90);
        let over = classified_eip8130(U256::ZERO, u64::from(cap), sender, payer, false, None);
        assert_eq!(guard.try_admit(admission_for(&over).unwrap()), Err(LimitRejection::PayerLimit));
    }

    #[test]
    fn expiry_bucket_invalidates_nonce_zero_and_nonce_free() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let expiry = 1_000u64;

        let nonce_zero = classified_eip8130(
            U256::ZERO,
            0,
            Address::repeat_byte(1),
            Address::repeat_byte(1),
            false,
            Some(expiry),
        );
        let nonce_free = classified_eip8130(
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            Address::repeat_byte(2),
            Address::repeat_byte(2),
            false,
            Some(expiry),
        );
        let zero_hash = *nonce_zero.hash();
        let free_hash = *nonce_free.hash();
        guard.try_admit(admission_for(&nonce_zero).unwrap()).unwrap();
        guard.try_admit(admission_for(&nonce_free).unwrap()).unwrap();

        // A bucket that has not yet come due leaves both in place.
        assert!(
            guard.invalidate_exact([InvalidationKey::expiry_bucket(expiry + 10_000)]).is_empty()
        );
        assert_eq!(guard.len(), 2);

        // Firing their bucket evicts both, regardless of nonce kind — the one
        // expiry surface shared by sponsored nonce_key==0 and nonce-free txns.
        let mut dropped = guard.invalidate_exact([InvalidationKey::expiry_bucket(expiry)]);
        dropped.sort();
        let mut expected = vec![zero_hash, free_hash];
        expected.sort();
        assert_eq!(dropped, expected);
        assert!(guard.is_empty());
    }

    type IntegrationPool = BaseTransactionPool<
        MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        InMemoryBlobStore,
        BaseEvmConfig,
    >;

    /// Builds a fully-wired [`BaseTransactionPool`] (reth `Pool` + real
    /// `BaseTransactionValidator`) over a `MockEthProvider`, so the `add_transaction`
    /// path — validation, guard gating, and rollback — is exercised end to end.
    /// L1-data-gas checks are disabled so funding is just the tx cost.
    fn build_integration_pool() -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>)
    {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().cobalt_activated().build());
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
                BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
                    .require_l1_data_gas_fee(false)
            });
        let ordering = BaseOrdering::default();
        let pool = Pool::new(validator, ordering.clone(), blob_store, PoolConfig::default());
        (BaseTransactionPool::new(pool, ordering), client)
    }

    fn fund(client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>, account: Address) {
        client.add_account(
            account,
            ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)),
        );
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

    fn self_paid_eoa_8130(signer: &PrivateKeySigner, nonce_sequence: u64) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence,
            expiry: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000,
            gas_limit: 1_000_000,
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

    #[tokio::test]
    async fn standard_1559_transactions_are_not_8130_gated() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        // Submit well beyond the 8130 sender cap: standard transactions carry a
        // limit class but must not be charged the 8130 caps (reth owns their
        // sender/nonce gating), so every one is accepted.
        let count = u64::from(GuardLimits::default().default_sender) + 3;
        for nonce in 0..count {
            let result = pool.add_transaction(TransactionOrigin::Local, signed_1559(&signer, nonce)).await;
            assert!(result.is_ok(), "standard 1559 tx #{nonce} should not be 8130-gated: {result:?}");
        }
    }

    #[tokio::test]
    async fn self_paid_nonce_zero_8130_is_gated_by_sender_limit_end_to_end() {
        let (pool, client) = build_integration_pool();
        let signer = signer();
        fund(&client, signer.address());

        let cap = u64::from(GuardLimits::default().default_sender);
        // The first `cap` self-paid nonce_key==0 transactions are admitted.
        for seq in 0..cap {
            let result =
                pool.add_transaction(TransactionOrigin::Local, self_paid_eoa_8130(&signer, seq)).await;
            assert!(result.is_ok(), "8130 tx #{seq} within the cap should admit: {result:?}");
        }
        // The next one trips the sender dimension and is rolled back out of reth.
        let over = self_paid_eoa_8130(&signer, cap);
        let over_hash = *over.hash();
        let result = pool.add_transaction(TransactionOrigin::Local, over).await;
        let err = result.expect_err("over-cap 8130 tx must be rejected by the guard");
        assert!(
            err.to_string().contains("sender EIP-8130 mempool limit"),
            "unexpected rejection: {err}"
        );
        // Rollback leaves no trace: the rejected tx is not in the pool.
        assert!(pool.get(&over_hash).is_none());
    }
}
