//! Unified Base transaction pool backed by lane-aware storage.

use std::{collections::HashSet, fmt, sync::Arc, time::Instant};

use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{
    Address, B128, B256, TxHash, U256,
    map::{AddressMap, AddressSet, B256Map},
};
use parking_lot::{Mutex, RwLock};
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStore, BlobStoreError, BlockInfo, GetPooledTransactionLimit,
    NewBlobSidecar, NewTransactionEvent, Pool, PoolResult, PoolSize, PoolTransaction,
    PropagatedTransactions, TransactionEvents, TransactionListenerKind, TransactionOrigin,
    TransactionPool, TransactionPoolExt, TransactionValidationOutcome,
    TransactionValidationTaskExecutor, TransactionValidator, ValidPoolTransaction, ValidatingPool,
    error::{Eip7702PoolTransactionError, InvalidPoolTransactionError, PoolError, PoolErrorKind},
    identifier::{SenderId, TransactionId},
    validate::ValidTransaction,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    Admission, BasePooledTx, BaseTransactionIdentity, BaseTransactionLane,
    BaseTransactionValidator, BestLaneTransactions, BestTransactionPriority, BlockExpiryIndex,
    GuardLimits, GuardMetrics, InvalidationCause, InvalidationKey, LaneCanonicalAccountUpdate,
    LaneEventHub, LaneRemovalReason, LaneTerminalEvent, LaneTerminalTransition,
    LaneTransactionStore, LaneTransitionBatch, LaneTransitionCause, LimitRejection, MempoolGuard,
    ParkableBestTransactions, ParkableTransactionPool, ParkedBestTransactions,
    StateDiffInvalidation, StateDiffOrigin, ValidityPoolMetrics,
};

/// A per-account canonical state delta used for mempool invalidation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountStateDiff {
    /// Changed account and contract address for storage slots.
    pub address: Address,
    /// New balance, when changed.
    pub balance: Option<U256>,
    /// Whether the protocol nonce changed.
    pub nonce_changed: bool,
    /// Whether the bytecode hash changed.
    pub code_changed: bool,
    /// Changed contract storage slots.
    pub changed_slots: Vec<B256>,
}

impl AccountStateDiff {
    /// Creates an empty diff for `address`.
    #[must_use]
    pub fn new(address: Address) -> Self {
        Self { address, ..Default::default() }
    }

    fn exact_keys(&self) -> impl Iterator<Item = InvalidationKey> + '_ {
        self.nonce_changed
            .then_some(InvalidationKey::ProtocolNonce(self.address))
            .into_iter()
            .chain(self.code_changed.then_some(InvalidationKey::CodeHash(self.address)))
            .chain(
                self.changed_slots
                    .iter()
                    .map(|slot| InvalidationKey::Slot { address: self.address, slot: *slot }),
            )
    }
}

#[derive(Debug)]
struct UnifiedPoolState<T: BasePooledTx> {
    store: LaneTransactionStore<T>,
    events: Arc<LaneEventHub<T>>,
    guard: MempoolGuard,
    block_expiry: BlockExpiryIndex,
    senders: LaneSenderIdentifiers,
    speculatively_pruned: B256Map<SpeculativelyPruned<T>>,
    speculative_generation: u64,
    speculative_limit: usize,
    version: u64,
    block_info: BlockInfo,
    store_size: crate::LaneStoreSize,
}

#[derive(Debug)]
struct SpeculativelyPruned<T: BasePooledTx> {
    transaction: Arc<ValidPoolTransaction<T>>,
    generation: u64,
}

#[derive(Debug, Default)]
struct LaneSenderIdentifiers {
    next: u64,
    by_address: AddressMap<SenderId>,
}

impl LaneSenderIdentifiers {
    fn preview(
        &self,
        sender: Address,
        authorities: Option<&[Address]>,
    ) -> (SenderId, Option<Vec<SenderId>>) {
        let mut next = self.next;
        let mut provisional = AddressMap::default();
        let mut id_for = |address| {
            self.by_address.get(&address).copied().unwrap_or_else(|| {
                *provisional.entry(address).or_insert_with(|| {
                    let id = SenderId::from(next);
                    next = next.wrapping_add(1);
                    id
                })
            })
        };
        let sender_id = id_for(sender);
        let authority_ids = authorities
            .map(|authorities| authorities.iter().copied().map(&mut id_for).collect::<Vec<_>>());
        (sender_id, authority_ids)
    }

    fn sender_id_or_create(&mut self, address: Address) -> SenderId {
        *self.by_address.entry(address).or_insert_with(|| {
            let id = SenderId::from(self.next);
            self.next = self.next.wrapping_add(1);
            id
        })
    }

    fn sender_ids_or_create(
        &mut self,
        addresses: impl IntoIterator<Item = Address>,
    ) -> Vec<SenderId> {
        addresses.into_iter().map(|address| self.sender_id_or_create(address)).collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum InvalidBestIdentity {
    Lane(BaseTransactionLane),
    Replay(B256),
}

impl InvalidBestIdentity {
    const fn from_identity(identity: BaseTransactionIdentity) -> Self {
        match identity {
            BaseTransactionIdentity::Nonce { lane, .. } => Self::Lane(lane),
            BaseTransactionIdentity::Replay { replay_id } => Self::Replay(replay_id),
        }
    }
}

struct LiveBestTransactions<T, O>
where
    T: BasePooledTx,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
{
    state: Arc<RwLock<UnifiedPoolState<T>>>,
    current: BestLaneTransactions<T, O>,
    ordering: O,
    base_fee: u64,
    version: u64,
    updates: bool,
    allow_updates_out_of_order: bool,
    last_priority: Option<BestTransactionPriority<O::PriorityValue>>,
    known: HashSet<TxHash>,
    yielded: HashSet<TxHash>,
    yielded_identities: HashSet<BaseTransactionIdentity>,
    invalid: HashSet<InvalidBestIdentity>,
    skip_blobs: bool,
}

impl<T, O> fmt::Debug for LiveBestTransactions<T, O>
where
    T: BasePooledTx,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveBestTransactions").finish_non_exhaustive()
    }
}

impl<T, O> LiveBestTransactions<T, O>
where
    T: BasePooledTx,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
{
    fn new_current(state: Arc<RwLock<UnifiedPoolState<T>>>, ordering: O) -> Self {
        Self::build(state, ordering, None)
    }

    fn new(state: Arc<RwLock<UnifiedPoolState<T>>>, ordering: O, base_fee: u64) -> Self {
        Self::build(state, ordering, Some(base_fee))
    }

    fn build(state: Arc<RwLock<UnifiedPoolState<T>>>, ordering: O, base_fee: Option<u64>) -> Self {
        let guard = state.read();
        let base_fee = base_fee.unwrap_or(guard.block_info.pending_basefee);
        let version = guard.version;
        let known = guard
            .store
            .pending_transactions()
            .into_iter()
            .map(|transaction| *transaction.hash())
            .collect();
        let current = guard.store.best_transactions(ordering.clone(), base_fee);
        drop(guard);
        Self {
            state,
            current,
            ordering,
            base_fee,
            version,
            updates: true,
            allow_updates_out_of_order: false,
            last_priority: None,
            known,
            yielded: HashSet::new(),
            yielded_identities: HashSet::new(),
            invalid: HashSet::new(),
            skip_blobs: false,
        }
    }

    fn refresh(&mut self) {
        if !self.updates {
            return;
        }
        let state = self.state.read();
        if state.version == self.version {
            return;
        }
        self.current = state.store.best_transactions(self.ordering.clone(), self.base_fee);
        self.version = state.version;
    }
}

impl<T, O> Iterator for LiveBestTransactions<T, O>
where
    T: BasePooledTx,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.refresh();
            let Some(transaction) = self.current.next() else {
                if !self.updates || self.state.read().version == self.version {
                    return None;
                }
                continue;
            };
            let hash = *transaction.hash();
            let transaction_identity = transaction.transaction.identity();
            let identity = InvalidBestIdentity::from_identity(transaction_identity);
            if self.yielded.contains(&hash)
                || self.yielded_identities.contains(&transaction_identity)
                || self.invalid.contains(&identity)
            {
                continue;
            }
            let priority =
                BestTransactionPriority::new(&self.ordering, &transaction, self.base_fee);
            let live_update = self.known.insert(hash);
            if live_update
                && !self.allow_updates_out_of_order
                && self.last_priority.as_ref().is_some_and(|last| priority > *last)
            {
                self.invalid.insert(identity);
                continue;
            }
            if self.skip_blobs && transaction.transaction.is_eip4844() {
                self.invalid.insert(identity);
                continue;
            }
            self.yielded.insert(hash);
            self.yielded_identities.insert(transaction_identity);
            self.last_priority = Some(priority);
            return Some(transaction);
        }
    }
}

impl<T, O> BestTransactions for LiveBestTransactions<T, O>
where
    T: BasePooledTx,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
        self.invalid.insert(InvalidBestIdentity::from_identity(transaction.transaction.identity()));
    }

    fn no_updates(&mut self) {
        self.updates = false;
    }

    fn allow_updates_out_of_order(&mut self) {
        self.allow_updates_out_of_order = true;
    }

    fn skip_blobs(&mut self) {
        self.skip_blobs = true;
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.skip_blobs = skip_blobs;
    }
}

/// Base transaction pool with one ownership domain for every transaction class.
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
    service_pool:
        Pool<TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>, O, S>,
    ordering: O,
    state: Arc<RwLock<UnifiedPoolState<T>>>,
    admission_lock: Arc<Mutex<()>>,
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
            service_pool: self.service_pool.clone(),
            ordering: self.ordering.clone(),
            state: Arc::clone(&self.state),
            admission_lock: Arc::clone(&self.admission_lock),
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
    /// Creates a unified pool using the supplied Reth pool as validator, blob-store, and block-info
    /// service. Transactions are never inserted into the Reth pool.
    pub fn new(
        service_pool: Pool<
            TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>,
            O,
            S,
        >,
        ordering: O,
    ) -> Self {
        let config = service_pool.config().clone();
        let speculative_limit = config
            .pending_limit
            .max_txs
            .saturating_add(config.basefee_limit.max_txs)
            .saturating_add(config.queued_limit.max_txs)
            .clamp(1, 100_000);
        let block_info = service_pool.block_info();
        let base_fee = block_info.pending_basefee;
        Self {
            service_pool,
            ordering,
            state: Arc::new(RwLock::new(UnifiedPoolState {
                store: LaneTransactionStore::new(config, base_fee),
                events: Arc::new(LaneEventHub::default()),
                guard: MempoolGuard::unlimited(),
                block_expiry: BlockExpiryIndex::new(),
                senders: LaneSenderIdentifiers::default(),
                speculatively_pruned: B256Map::default(),
                speculative_generation: 0,
                speculative_limit,
                version: 0,
                block_info,
                store_size: crate::LaneStoreSize::default(),
            })),
            admission_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Configures EIP-8130 admission limits. Call before sharing the pool.
    #[must_use]
    pub fn with_guard_limits(self, limits: GuardLimits) -> Self {
        self.state.write().guard = MempoolGuard::new(limits);
        self
    }

    /// Returns the validator used for all transaction classes.
    pub fn validator(
        &self,
    ) -> &TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>> {
        self.service_pool.validator()
    }

    /// Builds admission metadata carried by a validated EIP-8130 transaction.
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
            payer_balance: class.payer_balance,
            max_cost: class.max_cost,
            priority: transaction.priority_fee_or_price(),
            watch_set,
        })
    }

    /// Applies canonical state changes and removes invalidated transactions.
    pub fn apply_state_diff(
        &self,
        diffs: &[AccountStateDiff],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.apply_state_diff_excluding(diffs, &[])
    }

    /// Applies canonical state changes without invalidating transactions mined by that update.
    pub fn apply_state_diff_excluding(
        &self,
        diffs: &[AccountStateDiff],
        mined_hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.apply_state_diff_with_origin(diffs, mined_hashes, StateDiffOrigin::Canonical)
    }

    fn apply_state_diff_with_origin(
        &self,
        diffs: &[AccountStateDiff],
        mined_hashes: &[TxHash],
        origin: StateDiffOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let _admission = self.admission_lock.lock();
        self.validator().validator().invalidate_limit_class_cache(diffs);
        let (removed, batches, skipped, events) = {
            let mut state = self.state.write();
            let mined_hashes = mined_hashes.iter().copied().collect::<HashSet<_>>();
            let (_, restored, skipped) = if origin == StateDiffOrigin::Canonical {
                self.reconcile_speculative_locked_excluding(&mut state, &mined_hashes)
            } else {
                (
                    0,
                    LaneTransitionBatch {
                        cause: LaneTransitionCause::Insert,
                        transitions: Vec::new(),
                    },
                    Vec::new(),
                )
            };
            let speculative_admissions = if origin == StateDiffOrigin::IntraBlock {
                state
                    .speculatively_pruned
                    .values()
                    .filter_map(|entry| Self::admission_for(&entry.transaction.transaction))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let exact = diffs.iter().flat_map(AccountStateDiff::exact_keys).collect::<Vec<_>>();
            let mut hashes = state.guard.invalidate_exact(exact);
            for diff in diffs {
                if let Some(balance) = diff.balance {
                    hashes.extend(state.guard.on_balance_changed(diff.address, balance));
                }
            }
            hashes.sort_unstable();
            hashes.dedup();
            hashes.retain(|hash| !mined_hashes.contains(hash));
            for admission in speculative_admissions {
                if let Ok(index) = hashes.binary_search(&admission.hash) {
                    hashes.remove(index);
                    state.guard.insert_forced(admission);
                }
            }
            let outcome =
                state.store.remove_exact_with_outcome(&hashes, LaneRemovalReason::Invalid);
            Self::cleanup_removed(&mut state, &outcome.removed);
            let mut batches = vec![restored, outcome.transitions];
            for diff in diffs {
                if let Some(balance) = diff.balance {
                    batches.push(state.store.set_payer_balance(diff.address, balance).transitions);
                }
            }
            state.store_size = state.store.size();
            if batches.iter().any(|batch| !batch.is_empty()) {
                state.version = state.version.wrapping_add(1);
            }
            (outcome.removed, batches, skipped, Arc::clone(&state.events))
        };
        Self::publish(&events, &batches);
        for hash in skipped {
            events.publish_discarded(hash);
        }
        GuardMetrics::record_state_diff_invalidations(removed.len());
        removed
    }

    /// Clears every guarded transaction and records the bulk invalidation cause.
    pub fn invalidate_all_tracked_transactions(
        &self,
        cause: InvalidationCause,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.invalidate_all_tracked_transactions_excluding(cause, &[])
    }

    /// Clears guarded transactions except hashes mined by the replacement canonical branch.
    pub fn invalidate_all_tracked_transactions_excluding(
        &self,
        cause: InvalidationCause,
        mined_hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let _admission = self.admission_lock.lock();
        self.validator().validator().clear_limit_class_cache();
        let (removed, batches, skipped, events) = {
            let mut state = self.state.write();
            let mined_hashes = mined_hashes.iter().copied().collect::<HashSet<_>>();
            let (_, restored, skipped) =
                self.reconcile_speculative_locked_excluding(&mut state, &mined_hashes);
            let mut hashes = state.guard.invalidate_all();
            hashes.retain(|hash| !mined_hashes.contains(hash));
            let outcome =
                state.store.remove_exact_with_outcome(&hashes, LaneRemovalReason::Invalid);
            Self::cleanup_removed(&mut state, &outcome.removed);
            state.store_size = state.store.size();
            if !outcome.transitions.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            (
                outcome.removed,
                vec![restored, outcome.transitions],
                skipped,
                Arc::clone(&state.events),
            )
        };
        Self::publish(&events, &batches);
        for hash in skipped {
            events.publish_discarded(hash);
        }
        GuardMetrics::record_bulk_invalidations(removed.len(), cause);
        removed
    }

    fn publish(events: &LaneEventHub<T>, batches: &[LaneTransitionBatch<T>]) {
        for batch in batches {
            events.publish(batch);
        }
    }

    fn cleanup_removed(state: &mut UnifiedPoolState<T>, removed: &[Arc<ValidPoolTransaction<T>>]) {
        for transaction in removed {
            state.guard.release(transaction.hash());
            state.block_expiry.remove(transaction.hash());
        }
    }

    fn reconcile_speculative_locked(
        &self,
        state: &mut UnifiedPoolState<T>,
    ) -> (usize, LaneTransitionBatch<T>, Vec<TxHash>) {
        self.reconcile_speculative_locked_excluding(state, &HashSet::new())
    }

    fn reconcile_speculative_locked_excluding(
        &self,
        state: &mut UnifiedPoolState<T>,
        excluded: &HashSet<TxHash>,
    ) -> (usize, LaneTransitionBatch<T>, Vec<TxHash>) {
        let mut entries = std::mem::take(&mut state.speculatively_pruned);
        let excluded_entries =
            entries.extract_if(|hash, _| excluded.contains(hash)).collect::<B256Map<_>>();
        state.speculatively_pruned = excluded_entries;
        if entries.is_empty() {
            return (
                0,
                LaneTransitionBatch { cause: LaneTransitionCause::Insert, transitions: Vec::new() },
                Vec::new(),
            );
        }
        debug_assert!(
            entries.values().all(|entry| { entry.generation <= state.speculative_generation })
        );
        let snapshot = state.store.snapshot();
        let outcome = state
            .store
            .restore_speculative(entries.into_values().map(|entry| entry.transaction).collect());
        for transaction in &outcome.skipped {
            state.guard.release(transaction.hash());
            state.block_expiry.remove(transaction.hash());
        }
        let eviction = state.store.enforce_limits_with_ordering(&self.ordering);
        Self::cleanup_removed(state, &eviction.removed);
        let evicted =
            eviction.removed.iter().map(|transaction| *transaction.hash()).collect::<HashSet<_>>();
        let terminals = eviction
            .removed
            .iter()
            .map(|transaction| LaneTerminalTransition {
                transaction: Arc::clone(transaction),
                terminal: LaneTerminalEvent::Evicted,
            })
            .collect();
        let transitions =
            state.store.transitions_since(snapshot, LaneTransitionCause::Insert, terminals);
        let skipped = outcome.skipped.iter().map(|transaction| *transaction.hash()).collect();
        let restored = outcome
            .restored
            .iter()
            .filter(|transaction| !evicted.contains(transaction.hash()))
            .count();
        state.speculative_generation = state.speculative_generation.wrapping_add(1);
        state.version = state.version.wrapping_add(1);
        state.store_size = state.store.size();
        (restored, transitions, skipped)
    }

    /// Restores transactions from the current abandoned speculative generation.
    pub fn reconcile_speculative_generation(&self) -> usize {
        let _admission = self.admission_lock.lock();
        let (restored, batch, skipped, events) = {
            let mut state = self.state.write();
            let (restored, batch, skipped) = self.reconcile_speculative_locked(&mut state);
            (restored, batch, skipped, Arc::clone(&state.events))
        };
        events.publish(&batch);
        for hash in skipped {
            events.publish_discarded(hash);
        }
        restored
    }

    fn limit_rejection_error(hash: TxHash, rejection: LimitRejection) -> PoolError {
        GuardMetrics::admission_rejected(GuardMetrics::rejection_reason(rejection)).increment(1);
        let reason = match rejection {
            LimitRejection::SenderLimit => "sender EIP-8130 signature limit reached",
            LimitRejection::PayerLimit => "payer EIP-8130 signature limit reached",
            LimitRejection::PaymentLimit => "payer EIP-8130 payment limit reached",
            LimitRejection::PayerBalance => "payer cannot fund another EIP-8130 transaction",
        };
        PoolError::other(hash, reason)
    }

    fn stale_classification_error(hash: TxHash) -> PoolError {
        PoolError::other(hash, "EIP-8130 admission classification changed during validation")
    }

    fn classification_is_current(&self, transaction: &T) -> bool {
        transaction.limit_class().is_none_or(|class| {
            class.classification_generation
                == self.validator().validator().limit_class_cache_generation()
        })
    }

    fn authority_error(hash: TxHash, error: Eip7702PoolTransactionError) -> PoolError {
        PoolError::new(
            hash,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Eip7702(error)),
        )
    }

    fn validate_authorities(
        &self,
        state: &UnifiedPoolState<T>,
        transaction: &ValidPoolTransaction<T>,
        state_nonce: u64,
        bytecode_hash: Option<B256>,
        replacing: Option<&Arc<ValidPoolTransaction<T>>>,
    ) -> PoolResult<()> {
        let limit = self.service_pool.config().max_inflight_delegated_slot_limit;
        if let Some(authorities) = &transaction.authority_ids {
            for authority in authorities {
                let mut active = state.store.transaction_count_by_sender_id(*authority);
                if replacing.is_some_and(|replaced| replaced.sender_id() == *authority) {
                    active = active.saturating_sub(1);
                }
                let speculative = state
                    .speculatively_pruned
                    .values()
                    .filter(|entry| entry.transaction.sender_id() == *authority)
                    .filter(|entry| {
                        replacing.is_none_or(|replaced| entry.transaction.hash() != replaced.hash())
                    })
                    .count();
                let count = active.saturating_add(speculative);
                if count > limit {
                    return Err(Self::authority_error(
                        *transaction.hash(),
                        Eip7702PoolTransactionError::AuthorityReserved,
                    ));
                }
            }
        }

        if !matches!(
            transaction.transaction.identity(),
            BaseTransactionIdentity::Nonce { lane: BaseTransactionLane::Protocol { .. }, .. }
        ) {
            return Ok(());
        }
        let delegated = bytecode_hash.is_some_and(|hash| hash != KECCAK_EMPTY)
            || state.store.has_authority(transaction.sender_id())
            || state.speculatively_pruned.values().any(|entry| {
                entry
                    .transaction
                    .authority_ids
                    .as_ref()
                    .is_some_and(|authorities| authorities.contains(&transaction.sender_id()))
            });
        if !delegated {
            return Ok(());
        }
        let active_pending = state
            .store
            .pending_protocol_transactions(transaction.sender())
            .into_iter()
            .filter(|candidate| {
                replacing.is_none_or(|replaced| candidate.hash() != replaced.hash())
            })
            .count();
        let speculative_pending = state
            .speculatively_pruned
            .values()
            .filter(|entry| entry.transaction.sender() == transaction.sender())
            .filter(|entry| entry.transaction.transaction.identity().is_protocol())
            .filter(|entry| {
                replacing.is_none_or(|replaced| entry.transaction.hash() != replaced.hash())
            })
            .count();
        let pending = active_pending.saturating_add(speculative_pending);
        if pending == 0 && transaction.nonce().saturating_sub(state_nonce) >= limit as u64 {
            return Err(Self::authority_error(
                *transaction.hash(),
                Eip7702PoolTransactionError::OutOfOrderTxFromDelegated,
            ));
        }
        if pending >= limit {
            return Err(Self::authority_error(
                *transaction.hash(),
                Eip7702PoolTransactionError::InflightTxLimitReached,
            ));
        }
        Ok(())
    }

    fn insert_validated_batch(
        &self,
        transactions: Vec<(TransactionOrigin, TransactionValidationOutcome<T>)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let _admission = self.admission_lock.lock();
        let (results, result_hashes, batch, direct_events, validity, events) = {
            let mut state = self.state.write();
            let block_info = state.block_info;
            let mut journal = state.store.mutation_journal();
            let mut results = Vec::with_capacity(transactions.len());
            let mut result_hashes = Vec::with_capacity(transactions.len());
            let mut terminals = Vec::new();
            let mut direct_events = Vec::new();
            let mut validity = Vec::new();

            for (origin, validated) in transactions {
                let TransactionValidationOutcome::Valid {
                    balance,
                    state_nonce,
                    transaction,
                    propagate,
                    bytecode_hash,
                    authorities,
                } = validated
                else {
                    match validated {
                        TransactionValidationOutcome::Invalid(transaction, error) => {
                            let hash = *transaction.hash();
                            direct_events.push((hash, true));
                            result_hashes.push(None);
                            results.push(Err(PoolError::new(hash, error)));
                        }
                        TransactionValidationOutcome::Error(hash, error) => {
                            direct_events.push((hash, false));
                            result_hashes.push(None);
                            results.push(Err(PoolError::other(hash, error)));
                        }
                        TransactionValidationOutcome::Valid { .. } => unreachable!(),
                    }
                    continue;
                };
                let transaction = match transaction {
                    ValidTransaction::Valid(transaction)
                    | ValidTransaction::ValidWithSidecar { transaction, .. } => transaction,
                };
                let hash = *transaction.hash();
                result_hashes.push(Some(hash));
                if !self.classification_is_current(&transaction) {
                    direct_events.push((hash, false));
                    results.push(Err(Self::stale_classification_error(hash)));
                    continue;
                }
                let identity = transaction.identity();
                let (sender_id, authority_ids) =
                    state.senders.preview(transaction.sender(), authorities.as_deref());
                let mut pooled = ValidPoolTransaction {
                    transaction,
                    transaction_id: TransactionId::new(sender_id, identity_nonce(identity)),
                    propagate,
                    timestamp: Instant::now(),
                    origin,
                    authority_ids,
                };
                if pooled.gas_limit() > block_info.block_gas_limit {
                    results.push(Err(PoolError::new(
                        hash,
                        InvalidPoolTransactionError::ExceedsGasLimit(
                            pooled.gas_limit(),
                            block_info.block_gas_limit,
                        ),
                    )));
                    continue;
                }
                let active_replaced = state.store.get_by_identity(&identity);
                let speculative_replaced = state
                    .speculatively_pruned
                    .iter()
                    .find(|(_, entry)| entry.transaction.transaction.identity() == identity)
                    .map(|(hash, entry)| (*hash, Arc::clone(&entry.transaction)));
                let replaced = active_replaced.as_ref().cloned().or_else(|| {
                    speculative_replaced.as_ref().map(|(_, transaction)| Arc::clone(transaction))
                });
                let speculative_sender_slots = state
                    .speculatively_pruned
                    .values()
                    .filter(|entry| entry.transaction.sender() == pooled.sender())
                    .count();
                if let Err(error) = self.validate_authorities(
                    &state,
                    &pooled,
                    state_nonce,
                    bytecode_hash,
                    replaced.as_ref(),
                ) {
                    results.push(Err(error));
                    continue;
                }
                if let Err(error) = state.store.preflight_insert_with_context(
                    &pooled,
                    state_nonce,
                    speculative_replaced.as_ref().map(|(_, transaction)| transaction),
                    speculative_sender_slots,
                ) {
                    results.push(Err(error));
                    continue;
                }
                state.store.journal_insertion(&mut journal, &pooled);

                if let Some(replaced) = &replaced {
                    state.guard.release(replaced.hash());
                }
                if let Some(admission) = Self::admission_for(&pooled.transaction) {
                    if replaced.is_some() {
                        state.guard.insert_forced(admission);
                    } else if let Err(rejection) = state.guard.try_admit(admission) {
                        results.push(Err(Self::limit_rejection_error(hash, rejection)));
                        continue;
                    }
                }

                let actual_sender_id = state.senders.sender_id_or_create(pooled.sender());
                let actual_authority_ids =
                    authorities.map(|authorities| state.senders.sender_ids_or_create(authorities));
                debug_assert_eq!(sender_id, actual_sender_id);
                debug_assert_eq!(pooled.authority_ids, actual_authority_ids);
                pooled.transaction_id =
                    TransactionId::new(actual_sender_id, identity_nonce(identity));
                pooled.authority_ids = actual_authority_ids;

                let payer = pooled.transaction.validated_funding().map(|funding| funding.payer());
                let payer_balance =
                    pooled.transaction.limit_class().map_or(balance, |class| class.payer_balance);
                if let Some(payer) = payer {
                    state.store.set_payer_balance_unbalanced(payer, payer_balance);
                }
                let expiry = crate::ValidityPredicate::block_expiry_bound(
                    pooled.transaction.validity_predicates(),
                );
                let is_validity = !pooled.transaction.validity_predicates().is_empty();
                if let Some((replaced_hash, replaced)) = speculative_replaced {
                    state.speculatively_pruned.remove(&replaced_hash);
                    state.block_expiry.remove(&replaced_hash);
                    terminals.push(LaneTerminalTransition {
                        transaction: replaced,
                        terminal: LaneTerminalEvent::Replaced { by: hash },
                    });
                }
                let insertion =
                    state.store.insert_preflighted_journaled(pooled, state_nonce, &journal);
                if let Some(replaced) = &insertion.replaced {
                    state.block_expiry.remove(replaced.hash());
                    terminals.push(LaneTerminalTransition {
                        transaction: Arc::clone(replaced),
                        terminal: LaneTerminalEvent::Replaced { by: hash },
                    });
                }
                if let Some(last_valid_block) = expiry {
                    state.block_expiry.insert(hash, last_valid_block);
                }
                state.speculatively_pruned.remove(&hash);
                validity.push((hash, is_validity, insertion.replaced.is_some()));
                results.push(Ok(insertion.outcome));
            }

            let mut store_size = state.store.size_after_journal(state.store_size, &journal);
            let evicted_transactions = state.store.enforce_limits_with_ordering_journal(
                &self.ordering,
                &mut journal,
                &mut store_size,
            );
            let evicted = evicted_transactions
                .iter()
                .map(|transaction| *transaction.hash())
                .collect::<std::collections::HashSet<_>>();
            for transaction in &evicted_transactions {
                state.guard.release(transaction.hash());
                state.block_expiry.remove(transaction.hash());
                terminals.push(LaneTerminalTransition {
                    transaction: Arc::clone(transaction),
                    terminal: LaneTerminalEvent::Evicted,
                });
            }
            for (result, hash) in results.iter_mut().zip(&result_hashes) {
                if result.is_ok() && hash.is_some_and(|hash| evicted.contains(&hash)) {
                    *result = Err(PoolError::new(
                        hash.expect("hash exists"),
                        PoolErrorKind::DiscardedOnInsert,
                    ));
                }
            }
            let batch = state.store.transitions_since_journal(
                journal,
                LaneTransitionCause::Insert,
                terminals,
            );
            state.store_size = store_size;
            if !batch.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            (results, result_hashes, batch, direct_events, validity, Arc::clone(&state.events))
        };
        events.publish(&batch);
        for (hash, invalid) in direct_events {
            if invalid {
                events.publish_invalid(hash);
            } else {
                events.publish_discarded(hash);
            }
        }
        for (hash, is_validity, replacement) in validity {
            if is_validity
                && results
                    .iter()
                    .zip(&result_hashes)
                    .any(|(result, result_hash)| result.is_ok() && *result_hash == Some(hash))
            {
                ValidityPoolMetrics::record_admission(replacement);
            }
        }
        results
    }

    fn remove_with_reason(
        &self,
        hashes: &[TxHash],
        descendants: bool,
        reason: LaneRemovalReason,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let _admission = self.admission_lock.lock();
        let (removed, batches, skipped, events) = {
            let mut state = self.state.write();
            let (_, restored, skipped) = self.reconcile_speculative_locked(&mut state);
            let outcome = if descendants {
                state.store.remove_with_descendants_outcome(hashes, reason)
            } else {
                state.store.remove_exact_with_outcome(hashes, reason)
            };
            Self::cleanup_removed(&mut state, &outcome.removed);
            state.store_size = state.store.size();
            if !outcome.transitions.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            (
                outcome.removed,
                vec![restored, outcome.transitions],
                skipped,
                Arc::clone(&state.events),
            )
        };
        Self::publish(&events, &batches);
        for hash in skipped {
            events.publish_discarded(hash);
        }
        removed
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
    fn invalidate_from_state_diff(
        &self,
        diffs: &[AccountStateDiff],
        mined_hashes: &[TxHash],
        origin: StateDiffOrigin,
    ) -> usize {
        self.apply_state_diff_with_origin(diffs, mined_hashes, origin).len()
    }

    fn invalidate_all_tracked(&self, cause: InvalidationCause) -> usize {
        self.invalidate_all_tracked_transactions(cause).len()
    }

    fn invalidate_all_tracked_excluding(
        &self,
        cause: InvalidationCause,
        mined_hashes: &[TxHash],
    ) -> usize {
        self.invalidate_all_tracked_transactions_excluding(cause, mined_hashes).len()
    }
}

impl<Client, S, Evm, T, O> ValidatingPool for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Validator = TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>;

    fn validator(&self) -> &Self::Validator {
        self.validator()
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
        let size = self.state.read().store_size;
        PoolSize {
            pending: size.pending,
            pending_size: size.pending_size,
            basefee: size.basefee,
            basefee_size: size.basefee_size,
            queued: size.queued,
            queued_size: size.queued_size,
            blob: 0,
            blob_size: 0,
            total: size.total,
        }
    }

    fn block_info(&self) -> BlockInfo {
        self.state.read().block_info
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: T,
    ) -> PoolResult<TransactionEvents> {
        let hash = *transaction.hash();
        let events = self.state.read().events.transaction_event_listener(hash);
        let validated = self.validator().validate_transaction(origin, transaction).await;
        self.insert_validated_batch(vec![(origin, validated)])
            .pop()
            .expect("single insertion returns one result")?;
        Ok(events)
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: T,
    ) -> PoolResult<AddedTransactionOutcome> {
        let validated = self.validator().validate_transaction(origin, transaction).await;
        self.insert_validated_batch(vec![(origin, validated)])
            .pop()
            .expect("single insertion returns one result")
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<T>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let validated =
            self.validator().validate_transactions_with_origin(origin, transactions).await;
        self.insert_validated_batch(
            validated.into_iter().map(|transaction| (origin, transaction)).collect(),
        )
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, T)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let origins = transactions.iter().map(|(origin, _)| *origin).collect::<Vec<_>>();
        let validated = self.validator().validate_transactions(transactions).await;
        self.insert_validated_batch(origins.into_iter().zip(validated).collect())
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        let state = self.state.read();
        (state.store.contains_hash(&tx_hash) || state.speculatively_pruned.contains_key(&tx_hash))
            .then(|| state.events.transaction_event_listener(tx_hash))
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<T> {
        self.state.read().events.all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash> {
        self.state.read().events.pending_transactions_listener_for(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> mpsc::Receiver<NewBlobSidecar> {
        self.service_pool.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        self.state.read().events.new_transactions_listener_for(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        self.state
            .read()
            .store
            .all_transactions()
            .into_iter()
            .filter(|transaction| transaction.propagate)
            .map(|transaction| *transaction.hash())
            .collect()
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        self.pooled_transaction_hashes().into_iter().take(max).collect()
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.state
            .read()
            .store
            .all_transactions()
            .into_iter()
            .filter(|transaction| transaction.propagate)
            .collect()
    }

    fn pooled_transactions_max(&self, max: usize) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pooled_transactions().into_iter().take(max).collect()
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<T as PoolTransaction>::Pooled> {
        let mut out = Vec::new();
        self.append_pooled_transaction_elements(&tx_hashes, limit, &mut out);
        out
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<T as PoolTransaction>::Pooled>,
    ) {
        let state = self.state.read();
        let mut size = 0;
        for hash in tx_hashes {
            let Some(transaction) = state.store.get_by_hash(hash) else { continue };
            if !transaction.propagate {
                continue;
            }
            let encoded_length = transaction.encoded_length();
            size += encoded_length;
            if limit.exceeds(size) {
                break;
            }
            if let Ok(transaction) = transaction.transaction.clone().try_into_pooled() {
                out.push(transaction.into_parts().0);
            }
        }
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<<T as PoolTransaction>::Pooled>> {
        self.state.read().store.get_by_hash(&tx_hash)?.transaction.clone().try_into_pooled().ok()
    }

    fn best_transactions(&self) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>> {
        Box::new(LiveBestTransactions::new_current(Arc::clone(&self.state), self.ordering.clone()))
    }

    fn best_transactions_with_attributes(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>> {
        Box::new(LiveBestTransactions::new(
            Arc::clone(&self.state),
            self.ordering.clone(),
            attributes.basefee,
        ))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.pending_transactions()
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let state = self.state.read();
        let transaction = state.store.protocol_transaction(sender, nonce)?;
        (state.store.state(&transaction.transaction.identity())
            == Some(crate::LaneTransactionState::Pending))
        .then_some(transaction)
    }

    fn pending_transactions_max(&self, max: usize) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions().into_iter().take(max).collect()
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let state = self.state.read();
        let mut transactions = state.store.basefee_transactions();
        transactions.extend(state.store.queued_transactions());
        transactions
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let size = self.state.read().store.size();
        (size.pending, size.basefee + size.queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions<T> {
        let state = self.state.read();
        let pending = state.store.pending_transactions();
        let mut queued = state.store.basefee_transactions();
        queued.extend(state.store.queued_transactions());
        AllPoolTransactions { pending, queued }
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        self.state
            .read()
            .store
            .all_transactions()
            .into_iter()
            .map(|transaction| *transaction.hash())
            .collect()
    }

    fn remove_transactions(&self, hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_with_reason(&hashes, false, LaneRemovalReason::Removed)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_with_reason(&hashes, true, LaneRemovalReason::Removed)
    }

    fn remove_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let _admission = self.admission_lock.lock();
        let (removed, batches, skipped, events) = {
            let mut state = self.state.write();
            let (_, restored, skipped) = self.reconcile_speculative_locked(&mut state);
            let outcome =
                state.store.remove_by_sender_with_outcome(sender, LaneRemovalReason::Removed);
            Self::cleanup_removed(&mut state, &outcome.removed);
            state.store_size = state.store.size();
            if !outcome.removed.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            (
                outcome.removed,
                vec![restored, outcome.transitions],
                skipped,
                Arc::clone(&state.events),
            )
        };
        Self::publish(&events, &batches);
        for hash in skipped {
            events.publish_discarded(hash);
        }
        removed
    }

    fn prune_transactions(&self, hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let _admission = self.admission_lock.lock();
        let (removed, batches, skipped, events) = {
            let mut state = self.state.write();
            let mut batches = Vec::new();
            let mut skipped = Vec::new();
            if state.speculatively_pruned.len().saturating_add(hashes.len())
                > state.speculative_limit
            {
                let (_, batch, reconcile_skipped) = self.reconcile_speculative_locked(&mut state);
                batches.push(batch);
                skipped = reconcile_skipped;
            }
            let outcome = state.store.prune_speculative(&hashes);
            let generation = state.speculative_generation;
            for transaction in &outcome.removed {
                state.speculatively_pruned.insert(
                    *transaction.hash(),
                    SpeculativelyPruned { transaction: Arc::clone(transaction), generation },
                );
            }
            state.store_size = state.store.size();
            if !outcome.removed.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            batches.push(outcome.transitions);
            (outcome.removed, batches, skipped, Arc::clone(&state.events))
        };
        Self::publish(&events, &batches);
        for hash in skipped {
            events.publish_discarded(hash);
        }
        removed
    }

    fn retain_unknown<A: HandleMempoolData>(&self, announcement: &mut A) {
        let state = self.state.read();
        announcement.retain_by_hash(|hash| !state.store.contains_hash(hash));
    }

    fn retain_contains<A: HandleMempoolData>(&self, announcement: &mut A) {
        let state = self.state.read();
        announcement.retain_by_hash(|hash| state.store.contains_hash(hash));
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.get_by_hash(tx_hash)
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let state = self.state.read();
        txs.into_iter().filter_map(|hash| state.store.get_by_hash(&hash)).collect()
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        let state = self.state.read();
        let propagated = PropagatedTransactions(
            txs.0.into_iter().filter(|(hash, _)| state.store.contains_hash(hash)).collect(),
        );
        state.events.publish_propagated(propagated);
    }

    fn get_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.transactions_by_sender(sender)
    }

    fn get_pending_transactions_with_predicate(
        &self,
        mut predicate: impl FnMut(&ValidPoolTransaction<T>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions()
            .into_iter()
            .filter(|transaction| predicate(transaction))
            .collect()
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.pending_transactions_by_sender(sender)
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.queued_transactions_by_sender(sender)
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.highest_protocol_transaction(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.highest_consecutive_protocol_transaction(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.state.read().store.protocol_transaction(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.state
            .read()
            .store
            .all_transactions()
            .into_iter()
            .filter(|transaction| transaction.origin == origin)
            .collect()
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions()
            .into_iter()
            .filter(|transaction| transaction.origin == origin)
            .collect()
    }

    fn unique_senders(&self) -> AddressSet {
        self.state.read().store.unique_senders().into_iter().collect()
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.service_pool.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        self.service_pool.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.service_pool.get_all_blobs_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        self.service_pool.get_blobs_for_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        self.service_pool.get_blobs_for_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
        self.service_pool.get_blobs_for_versioned_hashes_v3(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        indices_bitarray: B128,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        self.service_pool.get_blobs_for_versioned_hashes_v4(versioned_hashes, indices_bitarray)
    }

    fn has_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<bool>, BlobStoreError> {
        self.service_pool.has_blobs_for_versioned_hashes(versioned_hashes)
    }

    fn blob_store(&self) -> Box<dyn BlobStore> {
        Box::new(self.service_pool.blob_store().clone())
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
    fn begin_speculative_generation(&self) {
        let _admission = self.admission_lock.lock();
        let (batch, skipped, events) = {
            let mut state = self.state.write();
            if state.speculatively_pruned.is_empty() {
                state.speculative_generation = state.speculative_generation.wrapping_add(1);
                return;
            }
            let (_, batch, skipped) = self.reconcile_speculative_locked(&mut state);
            (batch, skipped, Arc::clone(&state.events))
        };
        events.publish(&batch);
        for hash in skipped {
            events.publish_discarded(hash);
        }
    }

    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> Box<dyn ParkableBestTransactions<T>> {
        let best = LiveBestTransactions::new(
            Arc::clone(&self.state),
            self.ordering.clone(),
            attributes.basefee,
        );
        Box::new(ParkedBestTransactions::new(best, self.ordering.clone(), attributes.basefee))
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
        let _admission = self.admission_lock.lock();
        self.service_pool.set_block_info(info);
        let (batches, events) = {
            let mut state = self.state.write();
            let (_, restored, skipped) = self.reconcile_speculative_locked(&mut state);
            state.block_info = info;
            let fee = state.store.set_base_fee(info.pending_basefee);
            let eviction = state.store.enforce_limits_with_ordering(&self.ordering);
            Self::cleanup_removed(&mut state, &eviction.removed);
            state.store_size = state.store.size();
            if !fee.transitions.is_empty() || !eviction.transitions.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            let events = Arc::clone(&state.events);
            (vec![restored, fee.transitions, eviction.transitions], (events, skipped))
        };
        Self::publish(&events.0, &batches);
        for hash in events.1 {
            events.0.publish_discarded(hash);
        }
    }

    fn on_canonical_state_change(
        &self,
        update: reth_transaction_pool::CanonicalStateUpdate<'_, Self::Block>,
    ) {
        let block_hash = update.hash();
        let block_number = update.number();
        let timestamp = update.timestamp();
        let mined = update.mined_transactions.clone();
        let accounts = update.changed_accounts.clone();
        let pending_base_fee = update.pending_block_base_fee;
        let block_info = update.block_info();
        let _admission = self.admission_lock.lock();
        self.service_pool.on_canonical_state_change(update);

        let (batches, pruned_mined, skipped, expiry_buckets, expired_count, events) = {
            let mut state = self.state.write();
            state.block_info = block_info;
            let pruned_mined = mined
                .iter()
                .filter_map(|hash| {
                    state.speculatively_pruned.remove(hash).map(|entry| (*hash, entry.transaction))
                })
                .collect::<Vec<_>>();
            for (hash, _) in &pruned_mined {
                state.guard.release(hash);
                state.block_expiry.remove(hash);
            }
            let (_, restored, skipped) = self.reconcile_speculative_locked(&mut state);
            let mut invalidated = Vec::new();
            for account in &accounts {
                invalidated
                    .extend(state.guard.on_balance_changed(account.address, account.balance));
            }
            invalidated.sort_unstable();
            invalidated.dedup();
            let account_updates = accounts
                .iter()
                .map(|account| LaneCanonicalAccountUpdate {
                    address: account.address,
                    nonce: account.nonce,
                    balance: account.balance,
                })
                .collect::<Vec<_>>();
            let horizon = timestamp / InvalidationKey::EXPIRY_BUCKET_SECS;
            let (mut expired, expiry_buckets) =
                state.guard.invalidate_expiry_buckets_through(horizon);
            expired.extend(state.block_expiry.drain_expired(block_number));
            expired.extend(state.store.expired_replay_hashes(timestamp.saturating_mul(1_000)));
            expired.sort_unstable();
            expired.dedup();
            let outcome = state.store.apply_canonical_update(
                pending_base_fee,
                &mined,
                block_hash,
                &account_updates,
                &invalidated,
                &expired,
            );
            Self::cleanup_removed(&mut state, &outcome.removed);
            state.store_size = state.store.size();
            if !outcome.transitions.is_empty() {
                state.version = state.version.wrapping_add(1);
            }
            let expired_count = outcome
                .removed
                .iter()
                .filter(|transaction| expired.contains(transaction.hash()))
                .count();
            (
                vec![restored, outcome.transitions],
                pruned_mined,
                skipped,
                expiry_buckets,
                expired_count,
                Arc::clone(&state.events),
            )
        };
        Self::publish(&events, &batches);
        for (hash, _) in pruned_mined {
            events.publish_mined(hash, block_hash);
        }
        for hash in skipped {
            events.publish_discarded(hash);
        }
        GuardMetrics::expiry_buckets_fired().increment(expiry_buckets as u64);
        GuardMetrics::record_expiry_invalidations(expired_count);
        GuardMetrics::tracked().set(self.state.read().guard.len() as f64);
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        let _admission = self.admission_lock.lock();
        let (batches, invalidated, events) = {
            let mut state = self.state.write();
            let mut hashes = Vec::new();
            for account in &accounts {
                hashes.extend(state.guard.on_balance_changed(account.address, account.balance));
            }
            hashes.sort_unstable();
            hashes.dedup();
            let invalidated =
                state.store.remove_exact_with_outcome(&hashes, LaneRemovalReason::Invalid);
            Self::cleanup_removed(&mut state, &invalidated.removed);
            let invalidated_count = invalidated.removed.len();
            let mut batches = vec![invalidated.transitions];
            for account in accounts {
                let cursor = state.store.set_lane_cursor(
                    BaseTransactionLane::Protocol { sender: account.address },
                    account.nonce,
                );
                Self::cleanup_removed(&mut state, &cursor.removed);
                batches.push(cursor.transitions);
                batches.push(
                    state.store.set_payer_balance(account.address, account.balance).transitions,
                );
            }
            state.store_size = state.store.size();
            if batches.iter().any(|batch| !batch.is_empty()) {
                state.version = state.version.wrapping_add(1);
            }
            (batches, invalidated_count, Arc::clone(&state.events))
        };
        Self::publish(&events, &batches);
        GuardMetrics::record_balance_update_invalidations(invalidated);
        if invalidated > 0 {
            debug!(count = invalidated, "transactions invalidated by account update");
        }
    }

    fn delete_blob(&self, tx: B256) {
        self.service_pool.delete_blob(tx);
    }

    fn delete_blobs(&self, txs: Vec<B256>) {
        self.service_pool.delete_blobs(txs);
    }

    fn cleanup_blobs(&self) {
        self.service_pool.cleanup_blobs();
    }
}

const fn identity_nonce(identity: BaseTransactionIdentity) -> u64 {
    match identity {
        BaseTransactionIdentity::Nonce { nonce, .. } => nonce,
        BaseTransactionIdentity::Replay { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_consensus::{
        SignableTransaction, TxEip1559,
        transaction::{Recovered, SignerRecoverable},
    };
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{B512, Bytes, TxKind};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BaseBlock, BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives,
        BaseTxEnvelope, Eip8130Constants, Eip8130Signed, TxEip8130,
    };
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_eip8130::{AccountConfigurationStorage, AccountState};
    use base_execution_evm::BaseEvmConfig;
    use futures::{FutureExt, StreamExt};
    use reth_primitives_traits::SealedBlock;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_tasks::Runtime;
    use reth_transaction_pool::{
        CanonicalStateUpdate, FullTransactionEvent, PoolConfig, PoolUpdateKind, PropagateKind,
        PropagatedTransactions, SubPool, SubPoolLimit, TransactionEvent,
        blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
    };

    use super::*;
    use crate::{BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, ValidatedFunding};

    type IntegrationPool = BaseTransactionPool<
        MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        InMemoryBlobStore,
        BaseEvmConfig,
    >;

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn build_pool(
        config: PoolConfig,
    ) -> (IntegrationPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
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
        let service_pool = Pool::new(validator, ordering.clone(), blob_store, config);
        (
            BaseTransactionPool::new(service_pool, ordering)
                .with_guard_limits(GuardLimits::default()),
            client,
        )
    }

    fn fund(client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>, address: Address) {
        client.add_account(
            address,
            ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)),
        );
    }

    fn signed_1559(
        signer: &PrivateKeySigner,
        nonce: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_1559_with_tip(signer, nonce, max_fee_per_gas, 0)
    }

    fn signed_1559_with_tip(
        signer: &PrivateKeySigner,
        nonce: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let transaction = TxEip1559 {
            chain_id: ChainConfig::mainnet().chain_id,
            nonce,
            gas_limit: 50_000,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&transaction.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(transaction.into_signed(signature));
        let recovered = envelope.clone().try_into_recovered().unwrap();
        BasePooledTransaction::new(recovered, envelope.encode_2718_len())
    }

    fn signed_8130(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        valid_before: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let transaction = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas,
            gas_limit: 1_000_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&transaction.sender_signature_hash()).unwrap();
        let signed = Eip8130Signed::new(
            transaction,
            Bytes::from(signature.as_bytes().to_vec()),
            Bytes::new(),
        );
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(
            ConsensusPooledTransaction::Eip8130(signed),
            signer.address(),
        ))
    }

    fn sponsored_8130(
        sender: &PrivateKeySigner,
        payer: &PrivateKeySigner,
        nonce: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let transaction = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: nonce,
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
        let sender_signature = sender.sign_hash_sync(&transaction.sender_signature_hash()).unwrap();
        let payer_signature =
            payer.sign_hash_sync(&transaction.payer_signature_hash(sender.address())).unwrap();
        let mut payer_auth = Vec::with_capacity(85);
        payer_auth.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        payer_auth.extend_from_slice(&payer_signature.r().to_be_bytes::<32>());
        payer_auth.extend_from_slice(&payer_signature.s().to_be_bytes::<32>());
        payer_auth.push(27 + u8::from(payer_signature.v()));
        let signed = Eip8130Signed::new(
            transaction,
            Bytes::from(sender_signature.as_bytes().to_vec()),
            Bytes::from(payer_auth),
        );
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(
            ConsensusPooledTransaction::Eip8130(signed),
            sender.address(),
        ))
    }

    fn configure_sponsors(
        client: &MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        payers: impl IntoIterator<Item = Address>,
    ) {
        let state =
            AccountState::from_word(U256::from(Eip8130Constants::SCOPE_SPONSOR_PAYER) << 232);
        client.add_account(
            AccountConfigurationStorage::ADDRESS,
            ExtendedAccount::new(0, U256::ZERO).extend_storage(payers.into_iter().map(|payer| {
                (AccountConfigurationStorage::account_state_slot(payer), state.to_word())
            })),
        );
    }

    #[tokio::test]
    async fn all_lane_classes_share_one_ownership_domain_and_protocol_views() {
        let (pool, client) = build_pool(PoolConfig::default());
        let protocol = signer();
        let channel = signer();
        let replay = signer();
        for address in [protocol.address(), channel.address(), replay.address()] {
            fund(&client, address);
        }

        let protocol_tx = signed_1559(&protocol, 0, 1_000);
        let channel_tx = signed_8130(&channel, U256::from(7), 0, 0, 900);
        let replay_tx = signed_8130(&replay, Eip8130Constants::NONCE_KEY_MAX, 0, 10, 800);
        let protocol_hash = *protocol_tx.hash();
        let channel_hash = *channel_tx.hash();
        let replay_hash = *replay_tx.hash();
        for transaction in [protocol_tx, channel_tx, replay_tx] {
            pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        }

        assert_eq!(pool.pool_size().total, 3);
        assert_eq!(pool.service_pool.pool_size().total, 0);
        assert_eq!(
            pool.get_transaction_by_sender_and_nonce(protocol.address(), 0).unwrap().hash(),
            &protocol_hash
        );
        assert!(pool.get_transaction_by_sender_and_nonce(channel.address(), 0).is_none());
        assert_eq!(
            pool.all_transaction_hashes().into_iter().collect::<std::collections::HashSet<_>>(),
            [protocol_hash, channel_hash, replay_hash].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn mixed_payer_key_zero_chain_survives_cross_type_replacement() {
        let (pool, client) = build_pool(PoolConfig::default());
        let sender = signer();
        let first_payer = signer();
        let second_payer = signer();
        for address in [sender.address(), first_payer.address(), second_payer.address()] {
            fund(&client, address);
        }
        configure_sponsors(&client, [first_payer.address(), second_payer.address()]);

        let first = sponsored_8130(&sender, &first_payer, 0, 1_000);
        let second = sponsored_8130(&sender, &second_payer, 1, 900);
        let first_hash = *first.hash();
        let second_hash = *second.hash();
        pool.add_transaction(TransactionOrigin::Local, first).await.unwrap();
        pool.add_transaction(TransactionOrigin::Local, second).await.unwrap();

        let ordinary = signed_1559(&sender, 0, 1_250);
        let ordinary_hash = *ordinary.hash();
        pool.add_transaction(TransactionOrigin::Local, ordinary).await.unwrap();

        assert!(pool.get(&first_hash).is_none());
        assert!(pool.get(&ordinary_hash).is_some());
        assert!(pool.get(&second_hash).is_some());
        assert_eq!(pool.pending_transactions().len(), 2);
        assert!(!pool.state.read().guard.contains(&first_hash));
        assert!(pool.state.read().guard.contains(&second_hash));
    }

    #[tokio::test]
    async fn unified_events_cover_fee_transitions_and_descendant_removal() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        let first = signed_1559(&signer, 0, 100);
        let second = signed_1559(&signer, 1, 100);
        let first_hash = *first.hash();
        let second_hash = *second.hash();
        let mut first_events =
            pool.add_transaction_and_subscribe(TransactionOrigin::Local, first).await.unwrap();
        pool.add_transaction(TransactionOrigin::Local, second).await.unwrap();
        let mut second_events = pool.transaction_event_listener(second_hash).unwrap();
        assert!(matches!(first_events.next().await, Some(TransactionEvent::Pending)));

        let mut info = pool.block_info();
        info.pending_basefee = 101;
        pool.set_block_info(info);
        assert!(matches!(first_events.next().await, Some(TransactionEvent::Queued)));
        assert!(matches!(second_events.next().await, Some(TransactionEvent::Queued)));

        let removed = pool.remove_transactions_and_descendants(vec![first_hash]);
        assert_eq!(removed.len(), 2);
        assert!(matches!(first_events.next().await, Some(TransactionEvent::Discarded)));
        assert!(matches!(second_events.next().await, Some(TransactionEvent::Discarded)));
    }

    #[tokio::test]
    async fn unified_limit_eviction_emits_one_terminal_event() {
        let config = PoolConfig {
            pending_limit: SubPoolLimit { max_txs: 1, max_size: usize::MAX },
            ..PoolConfig::default()
        };
        let (pool, client) = build_pool(config);
        let low = signer();
        let high = signer();
        fund(&client, low.address());
        fund(&client, high.address());
        let low_tx = signed_1559_with_tip(&low, 0, 100, 1);
        let low_hash = *low_tx.hash();
        let mut all_events = pool.all_transactions_event_listener();
        pool.add_transaction(TransactionOrigin::External, low_tx).await.unwrap();
        pool.add_transaction(TransactionOrigin::External, signed_1559_with_tip(&high, 0, 200, 10))
            .await
            .unwrap();

        assert!(pool.get(&low_hash).is_none());
        let mut discarded = 0;
        for _ in 0..3 {
            if let Ok(Some(FullTransactionEvent::Discarded(hash))) =
                tokio::time::timeout(Duration::from_millis(50), all_events.next()).await
                && hash == low_hash
            {
                discarded += 1;
            }
        }
        assert_eq!(discarded, 1);
        assert!(matches!(pool.pool_size().pending, 1));
        assert!(pool.get(&low_hash).is_none());
    }

    #[tokio::test]
    async fn sponsored_exact_balance_uses_pre_auth_payer_balance() {
        let (pool, client) = build_pool(PoolConfig::default());
        let sender = signer();
        let payer = signer();
        fund(&client, sender.address());
        fund(&client, payer.address());
        configure_sponsors(&client, [payer.address()]);

        let probe = sponsored_8130(&sender, &payer, 0, 1_000);
        let validated =
            pool.validator().validate_transaction(TransactionOrigin::Local, probe).await;
        let max_cost = validated
            .as_valid_transaction()
            .unwrap()
            .transaction()
            .validated_funding()
            .unwrap()
            .max_cost();
        client.add_account(payer.address(), ExtendedAccount::new(0, max_cost));

        let transaction = sponsored_8130(&sender, &payer, 0, 1_000);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        let state = pool.state.read();
        assert_eq!(state.store.payer_balance(payer.address()), Some(max_cost));
        assert_eq!(state.store.payer_reserved(payer.address()), max_cost);
        assert!(state.store.pending_transactions().iter().any(|tx| tx.hash() == &hash));
    }

    #[tokio::test]
    async fn failed_replacement_does_not_seed_new_payer_balance() {
        let (pool, client) = build_pool(PoolConfig::default());
        let sender = signer();
        let first_payer = signer();
        let second_payer = signer();
        for address in [sender.address(), first_payer.address(), second_payer.address()] {
            fund(&client, address);
        }
        configure_sponsors(&client, [first_payer.address(), second_payer.address()]);
        pool.add_transaction(
            TransactionOrigin::Local,
            sponsored_8130(&sender, &first_payer, 0, 1_000),
        )
        .await
        .unwrap();

        let error = pool
            .add_transaction(
                TransactionOrigin::Local,
                sponsored_8130(&sender, &second_payer, 0, 1_050),
            )
            .await
            .unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::ReplacementUnderpriced));
        assert_eq!(pool.state.read().store.payer_balance(second_payer.address()), None);
    }

    #[tokio::test]
    async fn batch_rewrites_only_evicted_successes() {
        let config = PoolConfig {
            pending_limit: SubPoolLimit { max_txs: 1, max_size: usize::MAX },
            ..PoolConfig::default()
        };
        let (pool, client) = build_pool(config);
        let low = signer();
        let high = signer();
        fund(&client, low.address());
        fund(&client, high.address());
        let low_hash = *signed_1559_with_tip(&low, 0, 100, 1).hash();
        let results = pool
            .add_transactions(
                TransactionOrigin::External,
                vec![
                    signed_1559_with_tip(&low, 0, 100, 1),
                    signed_1559_with_tip(&high, 0, 200, 10),
                ],
            )
            .await;
        assert!(matches!(
            &results[0],
            Err(PoolError { kind: PoolErrorKind::DiscardedOnInsert, .. })
        ));
        assert!(results[1].is_ok());
        assert!(pool.get(&low_hash).is_none());
    }

    #[tokio::test]
    async fn private_transactions_are_excluded_from_pooled_elements() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        let transaction = signed_1559(&signer, 0, 100);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Private, transaction).await.unwrap();
        assert!(pool.pooled_transaction_hashes().is_empty());
        assert!(
            pool.get_pooled_transaction_elements(vec![hash], GetPooledTransactionLimit::None)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn best_iterator_observes_live_updates_and_honors_no_updates() {
        let (pool, client) = build_pool(PoolConfig::default());
        let first = signer();
        let second = signer();
        fund(&client, first.address());
        fund(&client, second.address());

        let mut live = pool.best_transactions();
        pool.add_transaction(TransactionOrigin::Local, signed_1559(&first, 0, 100)).await.unwrap();
        assert_eq!(live.next().unwrap().sender(), first.address());

        let mut frozen = pool.best_transactions();
        frozen.no_updates();
        assert_eq!(frozen.next().unwrap().sender(), first.address());
        pool.add_transaction(TransactionOrigin::Local, signed_1559(&second, 0, 100)).await.unwrap();
        assert!(frozen.next().is_none());
    }

    #[tokio::test]
    async fn best_iterator_orders_live_updates_unless_explicitly_relaxed() {
        let (pool, client) = build_pool(PoolConfig::default());
        let low = signer();
        let high = signer();
        fund(&client, low.address());
        fund(&client, high.address());
        pool.add_transaction(TransactionOrigin::Local, signed_1559_with_tip(&low, 0, 100, 1))
            .await
            .unwrap();
        let mut ordered = pool.best_transactions();
        assert_eq!(ordered.next().unwrap().sender(), low.address());
        pool.add_transaction(TransactionOrigin::Local, signed_1559_with_tip(&high, 0, 100, 10))
            .await
            .unwrap();
        assert!(ordered.next().is_none());

        let third = signer();
        fund(&client, third.address());
        let mut relaxed = pool.best_transactions();
        relaxed.next();
        relaxed.allow_updates_out_of_order();
        pool.add_transaction(TransactionOrigin::Local, signed_1559_with_tip(&third, 0, 100, 20))
            .await
            .unwrap();
        assert_eq!(relaxed.next().unwrap().sender(), third.address());
    }

    #[tokio::test]
    async fn propagation_and_speculative_prune_preserve_hash_lifecycle() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        let transaction = signed_1559(&signer, 0, 100);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        let mut events = pool.transaction_event_listener(hash).unwrap();
        let mut propagated = PropagatedTransactions::default();
        propagated.record(hash, PropagateKind::Hash(B512::ZERO));
        pool.on_propagated(propagated);
        assert!(matches!(events.next().await, Some(TransactionEvent::Propagated(_))));

        assert_eq!(pool.prune_transactions(vec![hash]).len(), 1);
        assert!(tokio::time::timeout(Duration::from_millis(20), events.next()).await.is_err());
        assert!(pool.transaction_event_listener(hash).is_some());
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
        assert!(matches!(events.next().await, Some(TransactionEvent::Mined(_))));
    }

    #[tokio::test]
    async fn delegated_checks_use_protocol_lane_and_match_reth_authority_boundary() {
        let (pool, client) = build_pool(PoolConfig::default());
        let delegated = signer();
        let authority = signer();
        let submitter = signer();
        for address in [delegated.address(), authority.address(), submitter.address()] {
            fund(&client, address);
        }
        pool.add_transaction(
            TransactionOrigin::Local,
            signed_8130(&delegated, U256::from(1), 0, 0, 100),
        )
        .await
        .unwrap();
        pool.add_transaction(TransactionOrigin::Local, signed_1559(&authority, 0, 100))
            .await
            .unwrap();

        let authority_candidate = {
            let mut state = pool.state.write();
            let delegated_id = state.senders.sender_id_or_create(delegated.address());
            let authority_id = state.senders.sender_id_or_create(authority.address());
            let submitter_id = state.senders.sender_id_or_create(submitter.address());
            let delegated_candidate = ValidPoolTransaction {
                transaction: signed_1559(&delegated, 0, 100),
                transaction_id: TransactionId::new(delegated_id, 0),
                propagate: true,
                timestamp: Instant::now(),
                origin: TransactionOrigin::External,
                authority_ids: None,
            };
            pool.validate_authorities(
                &state,
                &delegated_candidate,
                0,
                Some(B256::repeat_byte(1)),
                None,
            )
            .unwrap();

            let authority_candidate = ValidPoolTransaction {
                transaction: signed_1559(&submitter, 0, 100),
                transaction_id: TransactionId::new(submitter_id, 0),
                propagate: true,
                timestamp: Instant::now(),
                origin: TransactionOrigin::External,
                authority_ids: Some(vec![authority_id]),
            };
            pool.validate_authorities(&state, &authority_candidate, 0, None, None).unwrap();
            authority_candidate
        };

        pool.add_transaction(TransactionOrigin::Local, signed_1559(&authority, 1, 100))
            .await
            .unwrap();
        let state = pool.state.read();
        assert!(matches!(
            pool.validate_authorities(&state, &authority_candidate, 0, None, None)
                .unwrap_err()
                .kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Eip7702(
                Eip7702PoolTransactionError::AuthorityReserved
            ))
        ));
    }

    #[tokio::test]
    async fn revalidation_rejects_stale_funding_metadata() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        let transaction = signed_1559(&signer, 0, 100);
        transaction
            .set_validated_funding(ValidatedFunding::new(signer.address(), U256::ONE))
            .unwrap();
        let error = pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::InvalidTransaction(_)));
    }

    #[tokio::test]
    async fn canonical_diff_never_invalidates_mined_key_zero_or_channel_hashes() {
        let (pool, client) = build_pool(PoolConfig::default());
        let key_zero = signer();
        let channel = signer();
        fund(&client, key_zero.address());
        fund(&client, channel.address());
        let key_zero_tx = signed_8130(&key_zero, U256::ZERO, 0, 0, 100);
        let channel_tx = signed_8130(&channel, U256::from(7), 0, 0, 100);
        let key_zero_hash = *key_zero_tx.hash();
        let channel_hash = *channel_tx.hash();
        pool.add_transaction(TransactionOrigin::Local, key_zero_tx).await.unwrap();
        pool.add_transaction(TransactionOrigin::Local, channel_tx).await.unwrap();
        let mut key_zero_events = pool.transaction_event_listener(key_zero_hash).unwrap();
        let mut channel_events = pool.transaction_event_listener(channel_hash).unwrap();
        let channel_slot = pool
            .get(&channel_hash)
            .unwrap()
            .transaction
            .watch_set()
            .unwrap()
            .iter()
            .find_map(|key| match key {
                InvalidationKey::Slot { address, slot } => Some((*address, *slot)),
                _ => None,
            })
            .unwrap();
        let removed = pool.apply_state_diff_excluding(
            &[
                AccountStateDiff {
                    address: key_zero.address(),
                    nonce_changed: true,
                    ..Default::default()
                },
                AccountStateDiff {
                    address: channel_slot.0,
                    changed_slots: vec![channel_slot.1],
                    ..Default::default()
                },
            ],
            &[key_zero_hash, channel_hash],
        );
        assert!(removed.is_empty());
        assert!(key_zero_events.next().now_or_never().is_none());
        assert!(channel_events.next().now_or_never().is_none());

        let block = SealedBlock::seal_slow(BaseBlock {
            header: alloy_consensus::Header { timestamp: 1, ..Default::default() },
            body: Default::default(),
        });
        pool.on_canonical_state_change(CanonicalStateUpdate {
            new_tip: &block,
            pending_block_base_fee: 0,
            pending_block_blob_fee: None,
            changed_accounts: Vec::new(),
            mined_transactions: vec![key_zero_hash, channel_hash],
            update_kind: PoolUpdateKind::Commit,
        });
        assert!(matches!(key_zero_events.next().await, Some(TransactionEvent::Mined(_))));
        assert!(matches!(channel_events.next().await, Some(TransactionEvent::Mined(_))));
    }

    #[tokio::test]
    async fn abandoned_speculative_generation_is_restored_and_bounded() {
        let (pool, client) = build_pool(PoolConfig::default());
        let first = signer();
        let second = signer();
        fund(&client, first.address());
        fund(&client, second.address());
        let first_tx = signed_1559(&first, 0, 100);
        let second_tx = signed_1559(&second, 0, 100);
        let first_hash = *first_tx.hash();
        let second_hash = *second_tx.hash();
        pool.add_transaction(TransactionOrigin::Local, first_tx).await.unwrap();
        pool.add_transaction(TransactionOrigin::Local, second_tx).await.unwrap();
        pool.state.write().speculative_limit = 1;

        assert_eq!(pool.prune_transactions(vec![first_hash]).len(), 1);
        assert_eq!(pool.state.read().speculatively_pruned.len(), 1);
        assert_eq!(pool.prune_transactions(vec![second_hash]).len(), 1);
        let state = pool.state.read();
        assert_eq!(state.speculatively_pruned.len(), 1);
        assert!(state.store.contains_hash(&first_hash));
        drop(state);

        assert_eq!(pool.reconcile_speculative_generation(), 1);
        let state = pool.state.read();
        assert!(state.speculatively_pruned.is_empty());
        assert!(state.store.contains_hash(&first_hash));
        assert!(state.store.contains_hash(&second_hash));
        drop(state);

        pool.prune_transactions(vec![first_hash]);
        assert_eq!(pool.remove_transactions(vec![first_hash]).len(), 1);
        assert!(!pool.state.read().speculatively_pruned.contains_key(&first_hash));
    }

    #[tokio::test]
    async fn intra_block_diff_keeps_active_speculative_generation_pruned() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        let transaction = signed_1559(&signer, 0, 100);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        pool.prune_transactions(vec![hash]);

        let removed = pool.apply_state_diff_with_origin(
            &[AccountStateDiff {
                address: Address::repeat_byte(0xaa),
                code_changed: true,
                ..Default::default()
            }],
            &[],
            StateDiffOrigin::IntraBlock,
        );
        assert!(removed.is_empty());
        let state = pool.state.read();
        assert!(state.speculatively_pruned.contains_key(&hash));
        assert!(!state.store.contains_hash(&hash));
    }

    #[tokio::test]
    async fn batch_events_reflect_only_final_pending_states() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        let mut events = pool.new_transactions_listener_for(TransactionListenerKind::All);
        let results = pool
            .add_transactions(
                TransactionOrigin::Local,
                vec![signed_1559(&signer, 1, 100), signed_1559(&signer, 0, 100)],
            )
            .await;
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(events.recv().await.unwrap().subpool, SubPool::Pending);
        assert_eq!(events.recv().await.unwrap().subpool, SubPool::Pending);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn queued_promotions_are_live_best_updates() {
        let (pool, client) = build_pool(PoolConfig::default());
        let low = signer();
        let promoted = signer();
        fund(&client, low.address());
        fund(&client, promoted.address());
        pool.add_transaction(TransactionOrigin::Local, signed_1559_with_tip(&low, 0, 100, 1))
            .await
            .unwrap();
        pool.add_transaction(TransactionOrigin::Local, signed_1559_with_tip(&promoted, 1, 100, 10))
            .await
            .unwrap();
        let mut best = pool.best_transactions();
        assert_eq!(best.next().unwrap().sender(), low.address());
        pool.add_transaction(TransactionOrigin::Local, signed_1559_with_tip(&promoted, 0, 100, 10))
            .await
            .unwrap();
        assert!(best.next().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn best_snapshot_uses_one_atomic_fee_and_store_view() {
        let (pool, client) = build_pool(PoolConfig::default());
        let signer = signer();
        fund(&client, signer.address());
        pool.add_transaction(TransactionOrigin::Local, signed_1559(&signer, 0, 150)).await.unwrap();
        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            for fee in [100, 200].into_iter().cycle().take(200) {
                let mut info = writer_pool.block_info();
                info.pending_basefee = fee;
                writer_pool.set_block_info(info);
                tokio::task::yield_now().await;
            }
        });
        for _ in 0..200 {
            let mut best =
                LiveBestTransactions::new_current(Arc::clone(&pool.state), pool.ordering.clone());
            best.no_updates();
            assert_eq!(best.next().is_some(), best.base_fee <= 150);
            tokio::task::yield_now().await;
        }
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn reorg_flush_never_invalidates_new_branch_mined_eip8130_classes() {
        let (pool, client) = build_pool(PoolConfig::default());
        let key_zero = signer();
        let channel = signer();
        let replay = signer();
        for address in [key_zero.address(), channel.address(), replay.address()] {
            fund(&client, address);
        }
        let transactions = [
            signed_8130(&key_zero, U256::ZERO, 0, 0, 100),
            signed_8130(&channel, U256::from(9), 0, 0, 100),
            signed_8130(&replay, Eip8130Constants::NONCE_KEY_MAX, 0, 10, 100),
        ];
        let hashes = transactions.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>();
        for transaction in transactions {
            pool.add_transaction(TransactionOrigin::Local, transaction).await.unwrap();
        }
        let mut listeners = hashes
            .iter()
            .map(|hash| pool.transaction_event_listener(*hash).unwrap())
            .collect::<Vec<_>>();

        assert!(
            pool.invalidate_all_tracked_transactions_excluding(InvalidationCause::Reorg, &hashes)
                .is_empty()
        );
        for listener in &mut listeners {
            let observed = listener.next().now_or_never();
            assert!(observed.is_none(), "unexpected pre-mining event: {observed:?}");
        }

        let block = SealedBlock::seal_slow(BaseBlock {
            header: alloy_consensus::Header { timestamp: 1, ..Default::default() },
            body: Default::default(),
        });
        pool.on_canonical_state_change(CanonicalStateUpdate {
            new_tip: &block,
            pending_block_base_fee: 0,
            pending_block_blob_fee: None,
            changed_accounts: Vec::new(),
            mined_transactions: hashes,
            update_kind: PoolUpdateKind::Commit,
        });
        for listener in &mut listeners {
            assert!(matches!(listener.next().await, Some(TransactionEvent::Mined(_))));
        }
    }

    #[tokio::test]
    async fn speculative_pruning_retains_guard_and_account_capacity() {
        let config = PoolConfig { max_account_slots: 1, ..PoolConfig::default() };
        let (pool, client) = build_pool(config);
        let sender = signer();
        fund(&client, sender.address());
        pool.state.write().guard =
            MempoolGuard::new(GuardLimits { signature_limit: 1, payment_limit: u32::MAX });
        let first = signed_8130(&sender, U256::from(1), 0, 0, 100);
        let first_hash = *first.hash();
        pool.add_transaction(TransactionOrigin::External, first).await.unwrap();
        pool.prune_transactions(vec![first_hash]);
        assert!(pool.state.read().guard.contains(&first_hash));

        let guard_error = pool
            .add_transaction(
                TransactionOrigin::External,
                signed_8130(&sender, U256::from(2), 0, 0, 100),
            )
            .await
            .unwrap_err();
        assert!(matches!(guard_error.kind, PoolErrorKind::Other(_)));
        let account_error = pool
            .add_transaction(
                TransactionOrigin::External,
                signed_8130(&sender, Eip8130Constants::NONCE_KEY_MAX, 0, 10, 100),
            )
            .await
            .unwrap_err();
        assert!(matches!(account_error.kind, PoolErrorKind::SpammerExceededCapacity(_)));
    }

    #[tokio::test]
    async fn speculative_restore_reapplies_ordered_subpool_limits() {
        let config = PoolConfig {
            pending_limit: SubPoolLimit { max_txs: 1, max_size: usize::MAX },
            ..PoolConfig::default()
        };
        let (pool, client) = build_pool(config);
        let low = signer();
        let high = signer();
        fund(&client, low.address());
        fund(&client, high.address());
        let low_tx = signed_1559_with_tip(&low, 0, 100, 1);
        let low_hash = *low_tx.hash();
        pool.add_transaction(TransactionOrigin::External, low_tx).await.unwrap();
        pool.prune_transactions(vec![low_hash]);
        pool.add_transaction(TransactionOrigin::External, signed_1559_with_tip(&high, 0, 100, 10))
            .await
            .unwrap();

        assert_eq!(pool.reconcile_speculative_generation(), 0);
        assert_eq!(pool.pool_size().total, 1);
        assert!(pool.get(&low_hash).is_none());
        assert!(!pool.state.read().guard.contains(&low_hash));
    }

    #[tokio::test]
    async fn speculative_nonce_free_replacement_at_capacity_is_safe() {
        let config = PoolConfig { max_account_slots: 1, ..PoolConfig::default() };
        let (pool, client) = build_pool(config);
        let sender = signer();
        fund(&client, sender.address());
        let first = signed_8130(&sender, Eip8130Constants::NONCE_KEY_MAX, 0, 10, 100);
        let replacement = signed_8130(&sender, Eip8130Constants::NONCE_KEY_MAX, 0, 10, 120);
        assert_eq!(first.identity(), replacement.identity());
        let first_hash = *first.hash();
        let replacement_hash = *replacement.hash();
        pool.add_transaction(TransactionOrigin::External, first).await.unwrap();
        pool.prune_transactions(vec![first_hash]);

        pool.add_transaction(TransactionOrigin::External, replacement).await.unwrap();
        assert!(pool.get(&first_hash).is_none());
        assert!(pool.get(&replacement_hash).is_some());
    }

    #[test]
    fn sender_identifier_preview_scales_without_state_cloning() {
        let mut identifiers = LaneSenderIdentifiers::default();
        for value in 0..10_000u64 {
            let address = Address::from_word(B256::from(U256::from(value)));
            let (preview, authorities) = identifiers.preview(address, None);
            assert!(authorities.is_none());
            assert_eq!(preview, identifiers.sender_id_or_create(address));
        }
        assert_eq!(identifiers.by_address.len(), 10_000);
        assert_eq!(identifiers.next, 10_000);
    }

    #[tokio::test]
    async fn intra_block_invalidation_preserves_speculative_guard_accounting() {
        let (pool, client) = build_pool(PoolConfig::default());
        let sender = signer();
        fund(&client, sender.address());
        pool.state.write().guard =
            MempoolGuard::new(GuardLimits { signature_limit: 1, payment_limit: u32::MAX });
        let transaction = signed_8130(&sender, U256::from(7), 0, 0, 100);
        let hash = *transaction.hash();
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
        let slot = pool
            .get(&hash)
            .unwrap()
            .transaction
            .watch_set()
            .unwrap()
            .iter()
            .find_map(|key| match key {
                InvalidationKey::Slot { address, slot } => Some((*address, *slot)),
                _ => None,
            })
            .unwrap();
        pool.prune_transactions(vec![hash]);

        let removed = pool.apply_state_diff_with_origin(
            &[AccountStateDiff {
                address: slot.0,
                changed_slots: vec![slot.1],
                ..Default::default()
            }],
            &[],
            StateDiffOrigin::IntraBlock,
        );
        assert!(removed.is_empty());
        assert!(pool.state.read().guard.contains(&hash));
        let error = pool
            .add_transaction(
                TransactionOrigin::External,
                signed_8130(&sender, U256::from(8), 0, 0, 100),
            )
            .await
            .unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::Other(_)));
    }

    #[tokio::test]
    async fn authority_checks_include_speculatively_pruned_transactions() {
        let (pool, client) = build_pool(PoolConfig::default());
        let authority = signer();
        let submitter = signer();
        fund(&client, authority.address());
        fund(&client, submitter.address());
        let first = signed_1559(&authority, 0, 100);
        let second = signed_1559(&authority, 1, 100);
        let hashes = [*first.hash(), *second.hash()];
        pool.add_transactions(TransactionOrigin::Local, vec![first, second])
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        pool.prune_transactions(hashes.to_vec());

        let mut state = pool.state.write();
        let authority_id = state.senders.sender_id_or_create(authority.address());
        let submitter_id = state.senders.sender_id_or_create(submitter.address());
        let candidate = ValidPoolTransaction {
            transaction: signed_1559(&submitter, 0, 100),
            transaction_id: TransactionId::new(submitter_id, 0),
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: Some(vec![authority_id]),
        };
        assert!(matches!(
            pool.validate_authorities(&state, &candidate, 0, None, None).unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Eip7702(
                Eip7702PoolTransactionError::AuthorityReserved
            ))
        ));
    }

    #[tokio::test]
    async fn insertion_avoids_whole_pool_classification_passes() {
        let (pool, client) = build_pool(PoolConfig::default());
        let mut transactions = Vec::new();
        for _ in 0..128 {
            let signer = signer();
            fund(&client, signer.address());
            transactions.push(signed_1559(&signer, 0, 100));
        }
        let results = pool.add_transactions(TransactionOrigin::External, transactions).await;
        assert!(results.iter().all(Result::is_ok));

        let final_signer = signer();
        fund(&client, final_signer.address());
        {
            let state = pool.state.read();
            state.store.reset_full_classification_passes();
            state.store.reset_incremental_scan_counts();
        }
        pool.add_transaction(TransactionOrigin::External, signed_1559(&final_signer, 0, 100))
            .await
            .unwrap();
        let state = pool.state.read();
        assert_eq!(state.store.full_classification_passes(), 0);
        let (lane_scans, payer_scans) = state.store.incremental_scan_counts();
        assert!(lane_scans <= 4, "unrelated lane scans: {lane_scans}");
        assert!(payer_scans <= 4, "unrelated payer scans: {payer_scans}");
    }
}
