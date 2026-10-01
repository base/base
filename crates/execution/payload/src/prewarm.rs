//! Opt-in concurrent predicate-state prewarming for payload builds.
//!
//! A payload build reads declared validity-predicate state (account balances and
//! storage slots) for transactions ahead of the build loop. On a cold
//! [`ExecutionCache`] those reads hit the database under build-latency pressure. This
//! module warms that state concurrently and read-only into the very same cache the
//! build reads through, so first-touch reads become cache hits.
//!
//! # Structure
//!
//! * [`PrewarmWorkerPool`] — owned by the payload builder and shared by all of its
//!   builds. Spawns a fixed, bounded set of IO worker threads once, off the hot path,
//!   and hands each build's [`PrewarmJob`] to the workers that are idle.
//! * [`PrewarmJob`] — one build's prewarming. Dropping it cancels queued work without
//!   waiting for worker IO.
//! * [`PrewarmScheduler`] — the build's key queue: deduplicates and caps the distinct
//!   state keys scheduled per build.
//! * [`PrewarmingBestTransactions`] — a [`ParkablePayloadTransactions`] adapter owning
//!   the build's main transaction iterator untouched, plus an independent read-only
//!   lookahead cursor opened with the same attributes. The cursor lives for the whole
//!   build, including flashblock refreshes, and advances once per consumed candidate
//!   after an initial bounded burst.
//!
//! # Invariants
//!
//! * The build's main iterator and its parking lifecycle are delegated to the inner
//!   adapter unchanged. The lookahead cursor is only ever advanced; it is never parked,
//!   promoted, invalidated, or committed.
//! * Scheduling never blocks the build loop on IO, and dropping a job never waits for
//!   workers.
//! * A build is dispatched only to idle workers, so no worker queues work for an old
//!   parent. Each worker opens its own [`CachedStateProvider`] inside its thread (it is
//!   `!Sync`) and drops it, and its cache handle, when the job closes. The engine skips
//!   execution-cache advancement while the shared cache has more than one handle, so
//!   this prompt release matters. After cancellation each worker finishes at most one
//!   in-flight read or provider open; queued keys are dropped immediately.
//! * Worker failures are logged and counted; they never fail the build, and warm
//!   results are never used for correctness decisions.
//!
//! # Configuration
//!
//! [`PrewarmConfig`] is opt-in. When the builder flag is enabled, the node bins also
//! enable the engine's `share_execution_cache_with_payload_builder` so builds receive
//! the shared cache; without a shared cache no prewarming runs.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};

use alloy_primitives::{Address, StorageKey, TxHash, U256, map::HashSet};
use base_execution_txpool::{BasePooledTx, ValidityPredicate};
use reth_execution_cache::{CachedStateProvider, ExecutionCache};
use reth_payload_util::PayloadTransactions;
use reth_storage_api::{AccountReader, StateProvider, StateProviderBox, errors::ProviderResult};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};
use tracing::warn;

use crate::{ParkablePayloadTransactions, metrics::PrewarmMetrics};

/// Where prewarm workers log and count.
const TARGET: &str = "payload_builder::prewarm";

/// Configuration for predicate-state prewarming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrewarmConfig {
    /// Whether prewarming runs at all. Disabled by default.
    pub enabled: bool,
    /// Number of IO worker threads in the shared pool.
    pub worker_count: usize,
    /// Transactions scanned ahead of the build loop in the initial burst; the cursor then
    /// advances once per consumed candidate.
    pub lookahead: usize,
    /// Maximum distinct state keys scheduled per build; once reached the scheduler
    /// saturates and stops advancing the lookahead cursor.
    pub key_cap: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self { enabled: false, worker_count: 2, lookahead: 1024, key_cap: 4096 }
    }
}

/// A warmable state location read by a declared validity predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WarmKey {
    /// Account balance.
    Balance(Address),
    /// Contract storage slot.
    Storage(Address, U256),
}

impl WarmKey {
    /// Returns the warmable state location of a predicate, if it reads state.
    pub const fn from_predicate(predicate: &ValidityPredicate) -> Option<Self> {
        match predicate {
            ValidityPredicate::Balance { address, .. } => Some(Self::Balance(*address)),
            ValidityPredicate::Storage { address, slot, .. } => {
                Some(Self::Storage(*address, *slot))
            }
            ValidityPredicate::BlockNumber { .. } | ValidityPredicate::FlashblockIndex { .. } => {
                None
            }
        }
    }

    /// Warms this key through a cache-filling provider.
    ///
    /// The storage slot is keyed exactly like the build loop's revm read
    /// (`B256::new(slot.to_be_bytes())`), so warmed entries turn the build's first-touch
    /// reads into shared-cache hits.
    pub fn warm<S: StateProvider>(&self, provider: &CachedStateProvider<S>) {
        match self {
            Self::Balance(address) => {
                PrewarmMetrics::reads_total("balance").increment(1);
                if let Err(error) = provider.basic_account(address) {
                    PrewarmMetrics::warm_errors_total().increment(1);
                    warn!(target: TARGET, error = %error, address = %address, "prewarm account read failed");
                }
            }
            Self::Storage(address, slot) => {
                PrewarmMetrics::reads_total("storage").increment(1);
                let storage_key = StorageKey::new(slot.to_be_bytes());
                if let Err(error) = provider.storage(*address, storage_key) {
                    PrewarmMetrics::warm_errors_total().increment(1);
                    warn!(target: TARGET, error = %error, address = %address, slot = ?slot, "prewarm storage read failed");
                }
            }
        }
    }
}

/// Guarded state of a [`PrewarmScheduler`].
#[derive(Debug, Default)]
pub struct SchedulerState {
    /// Keys queued and not yet taken by a worker.
    pub queued: VecDeque<WarmKey>,
    /// Every distinct key scheduled for this build, including ones already warmed.
    pub scheduled: HashSet<WarmKey>,
    /// Set once the build's job is dropped; a closed scheduler accepts and yields nothing.
    pub closed: bool,
}

/// One build's key queue, shared by its lookahead adapters (producers) and its prewarm
/// workers (consumers).
///
/// Producers never block on IO: each key takes one short mutex section. The queue is
/// bounded by `key_cap` because every queued key is also counted in `scheduled`.
#[derive(Debug)]
pub struct PrewarmScheduler {
    /// Queue, dedup set, and close flag.
    pub state: Mutex<SchedulerState>,
    /// Wakes workers waiting for keys or for the job to close.
    pub available: Condvar,
    /// Transactions scanned in the initial lookahead burst.
    pub lookahead: usize,
    /// Maximum distinct keys per build.
    pub key_cap: usize,
    /// Set when the key cap is reached or the job is closed; lets producers stop
    /// scanning without taking the lock.
    pub saturated: AtomicBool,
}

impl PrewarmScheduler {
    /// Creates a scheduler for one build.
    pub fn new(config: &PrewarmConfig) -> Self {
        Self {
            state: Mutex::default(),
            available: Condvar::new(),
            lookahead: config.lookahead,
            key_cap: config.key_cap,
            saturated: AtomicBool::new(false),
        }
    }

    /// Returns whether no further keys will be accepted for this build.
    pub fn is_saturated(&self) -> bool {
        self.saturated.load(Ordering::Relaxed)
    }

    /// Closes the scheduler: drops queued keys and wakes every waiting worker.
    pub fn close(&self) {
        self.saturated.store(true, Ordering::Relaxed);
        let mut state = self.state.lock().expect("prewarm scheduler mutex poisoned");
        state.closed = true;
        state.queued.clear();
        drop(state);
        self.available.notify_all();
    }

    /// Schedules one key, deduplicating against keys already scheduled for this build.
    ///
    /// Returns `false` once the scheduler is saturated (key cap reached or closed), so
    /// callers can stop scanning. Duplicates return `true`. Never blocks on IO.
    pub fn schedule_key(&self, key: WarmKey) -> bool {
        if self.is_saturated() {
            return false;
        }
        let mut state = self.state.lock().expect("prewarm scheduler mutex poisoned");
        if state.closed {
            return false;
        }
        if state.scheduled.len() >= self.key_cap {
            self.saturated.store(true, Ordering::Relaxed);
            PrewarmMetrics::keys_dropped_key_cap_total().increment(1);
            return false;
        }
        if !state.scheduled.insert(key) {
            PrewarmMetrics::keys_deduped_total().increment(1);
            return true;
        }
        state.queued.push_back(key);
        drop(state);
        self.available.notify_one();
        PrewarmMetrics::keys_scheduled_total().increment(1);
        true
    }

    /// Schedules the declared predicate state of one lookahead transaction, stopping at
    /// the first refused key. Context predicates read no state and produce no keys.
    pub fn schedule_transaction<T: BasePooledTx>(&self, transaction: &T) {
        PrewarmMetrics::transactions_scanned_total().increment(1);
        for key in transaction.validity_predicates().iter().filter_map(WarmKey::from_predicate) {
            if !self.schedule_key(key) {
                return;
            }
        }
    }

    /// Waits for the next key. Returns `None` once the scheduler is closed.
    pub fn next_key(&self) -> Option<WarmKey> {
        let mut state = self.state.lock().expect("prewarm scheduler mutex poisoned");
        loop {
            if state.closed {
                return None;
            }
            if let Some(key) = state.queued.pop_front() {
                return Some(key);
            }
            state = self.available.wait(state).expect("prewarm scheduler mutex poisoned");
        }
    }
}

/// One build's work for one pool worker.
pub struct WorkerJob {
    /// Opens the state provider for the job's exact parent state.
    pub provider_factory: Box<dyn Fn() -> ProviderResult<StateProviderBox> + Send>,
    /// The build's shared execution cache handle for this worker.
    pub cache: ExecutionCache,
    /// The build's key queue.
    pub scheduler: Arc<PrewarmScheduler>,
}

impl std::fmt::Debug for WorkerJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerJob").field("scheduler", &self.scheduler).finish_non_exhaustive()
    }
}

impl WorkerJob {
    /// Opens the provider, warms keys until the job closes, then drops the provider and
    /// cache handle before the worker becomes idle again.
    pub fn run(self) {
        let provider = match (self.provider_factory)() {
            Ok(state_provider) => CachedStateProvider::new_prewarm(state_provider, self.cache),
            Err(error) => {
                PrewarmMetrics::provider_open_errors_total().increment(1);
                warn!(target: TARGET, error = %error, "failed to open parent state provider for prewarm worker");
                return;
            }
        };
        while let Some(key) = self.scheduler.next_key() {
            key.warm(&provider);
        }
    }
}

/// Builder-owned pool of bounded prewarm IO workers, shared across all of its builds.
///
/// Threads are spawned once at pool creation and reused, so concurrent or rapidly
/// cancelled builds cannot accumulate threads. Each worker has a zero-capacity mailbox:
/// a job is handed over only to a worker that is idle and waiting, so a busy worker never
/// queues work for an old parent. A build that finds no idle worker skips prewarming.
/// Dropping the pool detaches its workers without waiting for IO.
#[derive(Debug)]
pub struct PrewarmWorkerPool {
    /// Configuration the pool and its schedulers are sized from.
    pub config: PrewarmConfig,
    /// One rendezvous mailbox per worker.
    pub workers: Vec<SyncSender<WorkerJob>>,
}

impl PrewarmWorkerPool {
    /// Creates the pool, spawning `config.worker_count` worker threads once. Disabled
    /// configurations spawn nothing.
    pub fn new(config: &PrewarmConfig) -> Self {
        let mut workers = Vec::new();
        if config.enabled {
            for index in 0..config.worker_count {
                let (sender, receiver) = sync_channel(0);
                match thread::Builder::new()
                    .name(format!("prewarm-worker-{index}"))
                    .spawn(move || Self::worker_loop(receiver))
                {
                    Ok(_) => workers.push(sender),
                    Err(error) => {
                        PrewarmMetrics::worker_spawn_errors_total().increment(1);
                        warn!(target: TARGET, error = %error, worker = index, "failed to spawn prewarm worker");
                    }
                }
            }
        }
        Self { config: *config, workers }
    }

    /// Starts a prewarm job for one build on every idle worker, or returns `None` when
    /// prewarming is disabled or no worker is idle.
    ///
    /// The factory is cloned per dispatched worker and invoked inside that worker's
    /// thread to open the state provider for the job's exact parent state.
    pub fn try_start_job<F>(&self, provider_factory: F, cache: ExecutionCache) -> Option<PrewarmJob>
    where
        F: Fn() -> ProviderResult<StateProviderBox> + Clone + Send + 'static,
    {
        if self.workers.is_empty() {
            return None;
        }
        PrewarmMetrics::jobs_total().increment(1);
        let scheduler = Arc::new(PrewarmScheduler::new(&self.config));
        let mut dispatched = false;
        for worker in &self.workers {
            let job = WorkerJob {
                provider_factory: Box::new(provider_factory.clone()),
                cache: cache.clone(),
                scheduler: Arc::clone(&scheduler),
            };
            dispatched |= worker.try_send(job).is_ok();
        }
        if !dispatched {
            PrewarmMetrics::jobs_skipped_busy_total().increment(1);
            return None;
        }
        Some(PrewarmJob { scheduler })
    }

    /// Worker body: runs jobs handed over through its mailbox and exits when the pool is
    /// dropped.
    pub fn worker_loop(jobs: Receiver<WorkerJob>) {
        while let Ok(job) = jobs.recv() {
            job.run();
        }
    }
}

/// One payload build's prewarm job. Dropping it closes the scheduler, cancelling queued
/// work and waking workers, without waiting for worker IO.
#[derive(Debug)]
pub struct PrewarmJob {
    /// The build's key queue, shared with the build's lookahead adapters.
    pub scheduler: Arc<PrewarmScheduler>,
}

impl Drop for PrewarmJob {
    fn drop(&mut self) {
        self.scheduler.close();
    }
}

/// Payload-transaction adapter that schedules bounded lookahead predicate-state warming
/// while the build loop consumes candidates.
///
/// The main transaction iterator is owned and delegated to untouched, so the build's
/// parking lifecycle and dynamic-inclusion semantics are exactly those of the inner
/// adapter. The independent lookahead cursor is opened with the same attributes as the
/// main iterator and is only ever advanced.
pub struct PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    /// The build's main transaction iterator, delegated to unchanged.
    pub inner: I,
    /// Independent read-only lookahead cursor over the same pool attributes. `None` when
    /// prewarming is inactive or the scheduler saturated.
    pub cursor: Option<Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>>,
    /// The build's prewarm scheduler. `None` when prewarming is inactive.
    pub prewarm: Option<Arc<PrewarmScheduler>>,
}

impl<I, T> std::fmt::Debug for PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrewarmingBestTransactions")
            .field("has_cursor", &self.cursor.is_some())
            .field("prewarm", &self.prewarm)
            .finish_non_exhaustive()
    }
}

impl<I, T> PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    /// Wraps the build's transaction iterator with prewarm lookahead scheduling.
    ///
    /// When `prewarm` is active, opens an independent read-only lookahead cursor from
    /// `pool` with the same `attributes` as the main iterator and schedules the initial
    /// bounded burst. When inactive, the adapter is a pure pass-through.
    pub fn new<P>(
        inner: I,
        pool: P,
        attributes: BestTransactionsAttributes,
        prewarm: Option<Arc<PrewarmScheduler>>,
    ) -> Self
    where
        P: TransactionPool<Transaction = T>,
    {
        let cursor = prewarm
            .as_ref()
            .filter(|scheduler| !scheduler.is_saturated())
            .map(|_| pool.best_transactions_with_attributes(attributes));
        Self::with_cursor(inner, cursor, prewarm)
    }

    /// Wraps an existing lookahead cursor and schedules the initial bounded burst.
    pub fn with_cursor(
        inner: I,
        cursor: Option<Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>>,
        prewarm: Option<Arc<PrewarmScheduler>>,
    ) -> Self {
        let mut adapter = Self { inner, cursor, prewarm };
        let initial = adapter.prewarm.as_ref().map_or(0, |scheduler| scheduler.lookahead);
        adapter.advance_lookahead(initial);
        adapter
    }

    /// Replaces the main transaction iterator, keeping the lookahead cursor and scheduler.
    ///
    /// Flashblock refreshes use this so each block scans the pool ahead once instead of
    /// rescanning it from the top on every flashblock.
    pub fn refresh(&mut self, inner: I) {
        self.inner = inner;
    }

    /// Advances the lookahead cursor by at most `budget` transactions, scheduling each
    /// transaction's declared predicate state. Drops the cursor once the scheduler
    /// saturates; keeps an empty cursor, which yields transactions that arrive later.
    pub fn advance_lookahead(&mut self, budget: usize) {
        let Some(scheduler) = self.prewarm.as_ref() else { return };
        for _ in 0..budget {
            if scheduler.is_saturated() {
                self.cursor = None;
                return;
            }
            let Some(transaction) = self.cursor.as_mut().and_then(Iterator::next) else {
                return;
            };
            scheduler.schedule_transaction(&transaction.transaction);
        }
    }
}

impl<I, T> PayloadTransactions for PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    type Transaction = T;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        let transaction = self.inner.next(ctx)?;
        self.advance_lookahead(1);
        Some(transaction)
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);
    }
}

impl<I, T> ParkablePayloadTransactions for PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    fn park_current(&mut self) -> bool {
        self.inner.park_current()
    }

    fn mark_current_committed(&mut self) {
        self.inner.mark_current_committed();
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.discard_parked(transaction_hash)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::AtomicUsize,
        time::{Duration, Instant},
    };

    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use base_common_consensus::BasePooledTransaction as ConsensusPooledTransaction;
    use base_execution_txpool::{BasePooledTransaction, ValidityOperator};
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_storage_api::BlockHashReader;
    use reth_transaction_pool::{
        TransactionOrigin, error::InvalidPoolTransactionError, identifier::TransactionId,
    };

    use super::*;

    fn enabled_config(workers: usize, lookahead: usize, key_cap: usize) -> PrewarmConfig {
        PrewarmConfig { enabled: true, worker_count: workers, lookahead, key_cap }
    }

    fn balance_predicate(address: Address) -> ValidityPredicate {
        ValidityPredicate::Balance { address, op: ValidityOperator::LessThan, value: U256::from(1) }
    }

    fn storage_predicate(address: Address, slot: U256) -> ValidityPredicate {
        ValidityPredicate::Storage {
            address,
            slot,
            mask: ValidityPredicate::default_mask(),
            op: ValidityOperator::LessThan,
            value: U256::from(1),
        }
    }

    fn validity_transaction(
        sender: Address,
        predicates: Vec<ValidityPredicate>,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(sender),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signed = tx.into_signed(Signature::new(U256::from(1), U256::from(1), false));
        let pooled = ConsensusPooledTransaction::Legacy(signed);
        let encoded_length = pooled.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(pooled.into(), sender),
            encoded_length,
        )
        .with_validity_predicates(predicates);
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), 0),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    /// Read-only state provider stub over a [`MockEthProvider`] that delays and counts
    /// account and storage reads (the reads prewarming is meant to remove).
    #[derive(Debug, Clone)]
    struct DelayStateProvider {
        inner: MockEthProvider,
        delay: Duration,
        account_reads: Arc<AtomicUsize>,
        storage_reads: Arc<AtomicUsize>,
    }

    impl DelayStateProvider {
        fn new(inner: MockEthProvider, delay: Duration) -> Self {
            Self {
                inner,
                delay,
                account_reads: Arc::new(AtomicUsize::new(0)),
                storage_reads: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn account_reads(&self) -> usize {
            self.account_reads.load(Ordering::Relaxed)
        }

        fn storage_reads(&self) -> usize {
            self.storage_reads.load(Ordering::Relaxed)
        }

        fn delay_once(&self) {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
        }
    }

    impl reth_storage_api::AccountReader for DelayStateProvider {
        fn basic_account(
            &self,
            address: &Address,
        ) -> reth_storage_api::errors::ProviderResult<Option<reth_primitives_traits::Account>>
        {
            self.account_reads.fetch_add(1, Ordering::Relaxed);
            self.delay_once();
            self.inner.basic_account(address)
        }
    }

    impl BlockHashReader for DelayStateProvider {
        fn block_hash(
            &self,
            number: alloy_primitives::BlockNumber,
        ) -> reth_storage_api::errors::ProviderResult<Option<B256>> {
            self.inner.block_hash(number)
        }

        fn canonical_hashes_range(
            &self,
            start: alloy_primitives::BlockNumber,
            end: alloy_primitives::BlockNumber,
        ) -> reth_storage_api::errors::ProviderResult<Vec<B256>> {
            self.inner.canonical_hashes_range(start, end)
        }
    }

    impl reth_storage_api::BytecodeReader for DelayStateProvider {
        fn bytecode_by_hash(
            &self,
            code_hash: &B256,
        ) -> reth_storage_api::errors::ProviderResult<Option<reth_primitives_traits::Bytecode>>
        {
            self.inner.bytecode_by_hash(code_hash)
        }
    }

    impl reth_storage_api::StateRootProvider for DelayStateProvider {
        fn state_root(
            &self,
            hashed_state: reth_trie_common::HashedPostState,
        ) -> reth_storage_api::errors::ProviderResult<B256> {
            self.inner.state_root(hashed_state)
        }

        fn state_root_from_nodes(
            &self,
            input: reth_trie_common::TrieInput,
        ) -> reth_storage_api::errors::ProviderResult<B256> {
            self.inner.state_root_from_nodes(input)
        }

        fn state_root_with_updates(
            &self,
            hashed_state: reth_trie_common::HashedPostState,
        ) -> reth_storage_api::errors::ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)>
        {
            self.inner.state_root_with_updates(hashed_state)
        }

        fn state_root_from_nodes_with_updates(
            &self,
            input: reth_trie_common::TrieInput,
        ) -> reth_storage_api::errors::ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)>
        {
            self.inner.state_root_from_nodes_with_updates(input)
        }
    }

    impl reth_storage_api::StorageRootProvider for DelayStateProvider {
        fn storage_root(
            &self,
            address: Address,
            hashed_storage: reth_trie_common::HashedStorage,
        ) -> reth_storage_api::errors::ProviderResult<B256> {
            self.inner.storage_root(address, hashed_storage)
        }

        fn storage_proof(
            &self,
            address: Address,
            slot: B256,
            hashed_storage: reth_trie_common::HashedStorage,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::StorageProof> {
            self.inner.storage_proof(address, slot, hashed_storage)
        }

        fn storage_multiproof(
            &self,
            address: Address,
            slots: &[B256],
            hashed_storage: reth_trie_common::HashedStorage,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::StorageMultiProof> {
            self.inner.storage_multiproof(address, slots, hashed_storage)
        }
    }

    impl reth_storage_api::StateProofProvider for DelayStateProvider {
        fn proof(
            &self,
            input: reth_trie_common::TrieInput,
            address: Address,
            slots: &[B256],
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::AccountProof> {
            self.inner.proof(input, address, slots)
        }

        fn multiproof(
            &self,
            input: reth_trie_common::TrieInput,
            targets: reth_trie_common::MultiProofTargets,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::MultiProof> {
            self.inner.multiproof(input, targets)
        }

        fn witness(
            &self,
            input: reth_trie_common::TrieInput,
            target: reth_trie_common::HashedPostState,
            mode: reth_trie_common::ExecutionWitnessMode,
        ) -> reth_storage_api::errors::ProviderResult<Vec<alloy_primitives::Bytes>> {
            self.inner.witness(input, target, mode)
        }
    }

    impl reth_storage_api::HashedPostStateProvider for DelayStateProvider {
        fn hashed_post_state(
            &self,
            bundle_state: &revm::database::BundleState,
        ) -> reth_storage_api::errors::ProviderResult<reth_trie_common::HashedPostState> {
            self.inner.hashed_post_state(bundle_state)
        }
    }

    impl StateProvider for DelayStateProvider {
        fn storage(
            &self,
            account: Address,
            storage_key: StorageKey,
        ) -> reth_storage_api::errors::ProviderResult<Option<alloy_primitives::StorageValue>>
        {
            self.storage_reads.fetch_add(1, Ordering::Relaxed);
            self.delay_once();
            self.inner.storage(account, storage_key)
        }
    }

    /// Builds a prewarm pool plus one shared cache, and a provider factory that opens a
    /// delaying provider over `mock`.
    fn test_pool(
        config: &PrewarmConfig,
        mock: &MockEthProvider,
        delay: Duration,
    ) -> (
        PrewarmWorkerPool,
        ExecutionCache,
        impl Fn() -> ProviderResult<StateProviderBox> + Clone + Send + 'static,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let provider = DelayStateProvider::new(mock.clone(), delay);
        let account_reads = Arc::clone(&provider.account_reads);
        let storage_reads = Arc::clone(&provider.storage_reads);
        let pool = PrewarmWorkerPool::new(config);
        let cache = ExecutionCache::new(1000);
        let factory = move || Ok(Box::new(provider.clone()) as StateProviderBox);
        (pool, cache, factory, account_reads, storage_reads)
    }

    fn warmable_account(mock: &MockEthProvider, address: Address) -> StorageKey {
        let storage_key = StorageKey::new(U256::from(7).to_be_bytes());
        mock.extend_accounts(vec![(
            address,
            ExtendedAccount::new(1, U256::from(100))
                .extend_storage(vec![(storage_key, U256::from(42))]),
        )]);
        storage_key
    }

    fn wait_until(condition: impl Fn() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    /// Starts a job once a worker is idle: freshly spawned or just-released workers take
    /// a moment to reach their mailbox.
    fn start_job(
        pool: &PrewarmWorkerPool,
        factory: impl Fn() -> ProviderResult<StateProviderBox> + Clone + Send + 'static,
        cache: &ExecutionCache,
    ) -> PrewarmJob {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(job) = pool.try_start_job(factory.clone(), cache.clone()) {
                return job;
            }
            assert!(Instant::now() < deadline, "no prewarm worker became idle");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn queued(scheduler: &PrewarmScheduler) -> usize {
        scheduler.state.lock().unwrap().queued.len()
    }

    fn released(cache: ExecutionCache) -> bool {
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        wait_until(|| saved.is_available(), Duration::from_secs(5))
    }

    fn slots_cached(
        cache: &ExecutionCache,
        address: Address,
        slots: impl Iterator<Item = usize>,
    ) -> bool {
        slots.into_iter().all(|slot| {
            cache
                .get_or_try_insert_storage_with(
                    address,
                    StorageKey::new(U256::from(slot).to_be_bytes()),
                    || Err::<U256, ()>(()),
                )
                .is_ok()
        })
    }

    #[test]
    fn scheduler_dedups_and_saturates_at_key_cap() {
        let address = Address::with_last_byte(1);
        let scheduler = PrewarmScheduler::new(&enabled_config(1, 8, 2));

        assert!(scheduler.schedule_key(WarmKey::Balance(address)));
        assert!(scheduler.schedule_key(WarmKey::Balance(address)), "duplicates count as scheduled");
        assert_eq!(queued(&scheduler), 1, "duplicate key must not be queued twice");

        assert!(scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(2))));
        assert!(!scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(3))));
        assert!(scheduler.is_saturated(), "key cap must saturate the scheduler");
        assert_eq!(queued(&scheduler), 2);
    }

    #[test]
    fn disabled_pool_spawns_no_workers_and_starts_no_jobs() {
        let pool = PrewarmWorkerPool::new(&PrewarmConfig::default());
        assert_eq!(pool.workers.len(), 0);
        assert!(
            pool.try_start_job(
                || Ok(Box::new(MockEthProvider::default()) as StateProviderBox),
                ExecutionCache::new(1000)
            )
            .is_none()
        );
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn busy_worker_skips_new_jobs_without_retaining_their_cache() {
        let pool = PrewarmWorkerPool::new(&enabled_config(1, 8, 64));
        let release = Arc::new(std::sync::Barrier::new(2));
        let first = start_job(
            &pool,
            {
                let release = Arc::clone(&release);
                move || {
                    release.wait();
                    Ok(Box::new(MockEthProvider::default()) as StateProviderBox)
                }
            },
            &ExecutionCache::new(1000),
        );

        // The only worker is blocked opening the first build's provider: a new build
        // skips prewarming instead of queueing behind it with a different-parent cache.
        let second_cache = ExecutionCache::new(1000);
        assert!(
            pool.try_start_job(
                || Ok(Box::new(MockEthProvider::default()) as StateProviderBox),
                second_cache.clone(),
            )
            .is_none()
        );
        assert!(released(second_cache));

        // Once the first build ends, the worker serves new builds again.
        release.wait();
        drop(first);
        drop(start_job(
            &pool,
            || Ok(Box::new(MockEthProvider::default()) as StateProviderBox),
            &ExecutionCache::new(1000),
        ));
    }

    #[test]
    fn prewarm_fills_shared_cache_and_eliminates_backend_reads() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(9);
        warmable_account(&mock, address);

        let (pool, cache, factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &mock, Duration::ZERO);
        let job = start_job(&pool, factory, &cache);
        assert!(job.scheduler.schedule_key(WarmKey::Balance(address)));
        assert!(job.scheduler.schedule_key(WarmKey::Storage(address, U256::from(7))));
        assert!(wait_until(
            || slots_cached(&cache, address, std::iter::once(7)),
            Duration::from_secs(5)
        ));
        drop(job);

        // A build-side provider over a different (empty) backend must be served entirely
        // from the warmed shared cache, mirroring the build loop's revm read path.
        let build_backend = DelayStateProvider::new(MockEthProvider::default(), Duration::ZERO);
        let build_provider =
            CachedStateProvider::new(Box::new(build_backend.clone()), cache.clone(), None);
        let mut db = reth_revm::database::StateProviderDatabase::new(build_provider);
        let account = revm::Database::basic(&mut db, address).unwrap().expect("warmed account");
        assert_eq!(account.balance, U256::from(100));
        let value = revm::Database::storage(&mut db, address, U256::from(7)).unwrap();
        assert_eq!(value, U256::from(42));
        assert_eq!(build_backend.account_reads(), 0, "warmed account must not hit the backend");
        assert_eq!(build_backend.storage_reads(), 0, "warmed slot must not hit the backend");

        drop(db);
        // Workers release their cache handles when the job ends: only this handle remains.
        assert!(released(cache));
    }

    #[test]
    fn prewarm_tolerates_missing_parent_state() {
        let (pool, cache, _factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &MockEthProvider::default(), Duration::ZERO);
        let job = start_job(
            &pool,
            || Err(reth_storage_api::errors::ProviderError::FinalizedBlockNotFound),
            &cache,
        );
        drop(job);
        assert!(released(cache), "worker must release the cache after the error");
    }

    #[test]
    fn prewarm_cancellation_drops_queued_work_and_releases_cache() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(5);
        warmable_account(&mock, address);
        let delay = Duration::from_millis(250);
        let (pool, cache, factory, _account_reads, storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &mock, delay);
        let job = start_job(&pool, factory, &cache);
        for index in 0..3 {
            assert!(job.scheduler.schedule_key(WarmKey::Storage(address, U256::from(index))));
        }

        // Wait until the first (blocked) read is in flight, then cancel the job.
        assert!(wait_until(|| storage_reads.load(Ordering::Relaxed) == 1, Duration::from_secs(2)));
        let drop_start = Instant::now();
        drop(job);
        assert!(
            drop_start.elapsed() < delay / 2,
            "dropping the job must not wait for the in-flight read"
        );

        // The in-flight read finishes, and the worker then releases its cache handle and
        // drops the remaining queued keys.
        assert!(released(cache));
        assert_eq!(
            storage_reads.load(Ordering::Relaxed),
            1,
            "queued work must be cancelled, not warmed"
        );
    }

    /// Cursor double whose queue tests can extend, like a pool cursor seeing new arrivals.
    struct StaticCursor {
        transactions: Arc<Mutex<VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>>>,
    }

    impl StaticCursor {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self { transactions: Arc::new(Mutex::new(transactions.into())) }
        }
    }

    impl Iterator for StaticCursor {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.lock().unwrap().pop_front()
        }
    }

    impl BestTransactions for StaticCursor {
        fn mark_invalid(&mut self, _transaction: &Self::Item, _kind: InvalidPoolTransactionError) {}

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    /// Inner adapter double that yields a fixed candidate list.
    struct InnerStub(VecDeque<BasePooledTransaction>);

    impl InnerStub {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self(
                transactions
                    .into_iter()
                    .map(|transaction| transaction.transaction.clone())
                    .collect(),
            )
        }
    }

    impl PayloadTransactions for InnerStub {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            self.0.pop_front()
        }

        fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {}
    }

    impl ParkablePayloadTransactions for InnerStub {
        fn park_current(&mut self) -> bool {
            true
        }

        fn mark_current_committed(&mut self) {}

        fn promote(&mut self, _transaction_hash: TxHash) -> bool {
            true
        }

        fn discard_parked(&mut self, _transaction_hash: TxHash) -> bool {
            true
        }
    }

    #[test]
    fn adapter_schedules_initial_burst_then_one_advance_per_candidate() {
        let transactions: Vec<_> = (1..=4)
            .map(|index| {
                let sender = Address::with_last_byte(index);
                validity_transaction(
                    sender,
                    vec![balance_predicate(sender), storage_predicate(sender, U256::from(7))],
                )
            })
            .collect();

        let scheduler = Arc::new(PrewarmScheduler::new(&enabled_config(1, 2, 16)));
        let mut adapter =
            PrewarmingBestTransactions::<InnerStub, BasePooledTransaction>::with_cursor(
                InnerStub::new(transactions.clone()),
                Some(Box::new(StaticCursor::new(transactions))),
                Some(Arc::clone(&scheduler)),
            );

        // Initial bounded burst: lookahead transactions' keys are queued before the loop.
        assert_eq!(queued(&scheduler), 4, "two lookahead transactions, two keys each");

        // Each consumed candidate schedules exactly one further lookahead transaction.
        adapter.next(()).expect("candidate");
        assert_eq!(queued(&scheduler), 6);
        adapter.next(()).expect("candidate");
        assert_eq!(queued(&scheduler), 8);

        // An exhausted cursor schedules nothing further.
        assert!(adapter.next(()).is_some());
        assert!(adapter.next(()).is_some());
        assert!(adapter.next(()).is_none());
        assert_eq!(queued(&scheduler), 8);
    }

    #[test]
    fn refreshed_adapter_keeps_cursor_and_warms_late_arrivals() {
        let first = Address::with_last_byte(1);
        let late = Address::with_last_byte(2);
        let first_tx = validity_transaction(first, vec![balance_predicate(first)]);
        let late_tx = validity_transaction(late, vec![balance_predicate(late)]);

        let cursor = StaticCursor::new(vec![Arc::clone(&first_tx)]);
        let arrivals = Arc::clone(&cursor.transactions);
        let scheduler = Arc::new(PrewarmScheduler::new(&enabled_config(1, 4, 16)));
        let mut adapter =
            PrewarmingBestTransactions::<InnerStub, BasePooledTransaction>::with_cursor(
                InnerStub::new(vec![first_tx]),
                Some(Box::new(cursor)),
                Some(Arc::clone(&scheduler)),
            );
        assert_eq!(queued(&scheduler), 1, "initial burst drains the cursor");

        // A flashblock refresh swaps the main iterator but keeps the dry cursor, which
        // still warms transactions that arrive afterwards.
        adapter.refresh(InnerStub::new(vec![Arc::clone(&late_tx)]));
        arrivals.lock().unwrap().push_back(late_tx);
        assert!(adapter.next(()).is_some());
        assert_eq!(queued(&scheduler), 2);
    }

    #[test]
    fn saturated_adapter_stops_scanning() {
        let transactions: Vec<_> = (1..=3)
            .map(|index| {
                let sender = Address::with_last_byte(index);
                validity_transaction(
                    sender,
                    vec![balance_predicate(sender), storage_predicate(sender, U256::from(7))],
                )
            })
            .collect();

        // Three distinct keys fit; the fourth saturates the scheduler mid-burst.
        let scheduler = Arc::new(PrewarmScheduler::new(&enabled_config(1, 3, 3)));
        let adapter = PrewarmingBestTransactions::<InnerStub, BasePooledTransaction>::with_cursor(
            InnerStub::new(transactions.clone()),
            Some(Box::new(StaticCursor::new(transactions))),
            Some(Arc::clone(&scheduler)),
        );

        assert_eq!(queued(&scheduler), 3);
        assert!(adapter.cursor.is_none(), "a saturated build must stop scanning the pool");
    }
}
