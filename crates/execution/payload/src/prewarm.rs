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
//!   builds. Spawns a fixed, bounded set of IO worker threads once, off the hot path.
//!   Each build gets one [`PrewarmJob`] dispatched to idle workers.
//! * [`PrewarmJob`] — one build's prewarming: it holds the build's
//!   [`PrewarmScheduler`] and, when dropped, cancels remaining queued work without
//!   waiting for worker IO.
//! * [`PrewarmScheduler`] — extracts, deduplicates, and caps the distinct state keys
//!   scheduled per build and owns the bounded work queue.
//! * [`WarmJob`] — the unit of work dispatched to workers. Currently only
//!   [`WarmJob::Key`]; future warming modes (e.g. transaction simulation) extend this
//!   enum and the worker loop without changing queueing, scheduling, or cancellation.
//! * [`PrewarmingBestTransactions`] — a [`ParkablePayloadTransactions`] adapter owning
//!   the build's main transaction iterator untouched, plus an independent read-only
//!   lookahead cursor opened with the same attributes. It schedules at most an initial
//!   bounded burst plus one lookahead transaction per candidate the build consumes.
//!
//! # Invariants
//!
//! * The build's main iterator and its parking lifecycle (`park_current`,
//!   `mark_current_committed`, `promote`, `discard_parked`, `mark_invalid`) are
//!   delegated to the inner adapter unchanged. The lookahead cursor is a second,
//!   independent standard `best_transactions` iterator that is only ever advanced;
//!   it is never parked, promoted, invalidated, or committed.
//! * Scheduling never blocks the build loop: a full queue drops work instead of
//!   stalling, and dropping or finishing a build never waits on worker IO.
//! * Worker threads are bounded (one pool per builder, fixed size) and each worker
//!   constructs its own [`CachedStateProvider`] inside its thread (it is `!Sync`) for
//!   the job's exact parent state and cache, dropping both when the job ends. The
//!   engine skips execution-cache advancement while the shared cache has more than one
//!   handle, so this prompt release matters. Between cancellation and full cache
//!   availability there is an unavoidable tail of at most one in-flight blocking read
//!   per worker; queued work is dropped immediately on cancellation.
//! * Worker failures (provider open errors, failed reads, busy workers) are logged and
//!   counted; they never fail the build, and warm results are never used for
//!   correctness decisions.
//!
//! # Configuration and rollout
//!
//! [`PrewarmConfig`] is opt-in (`enabled: false` by default) and carries the IO worker
//! count, the bounded lookahead, and the per-build distinct-key cap. When the builder
//! flag is enabled, the node bins also enable the engine's
//! `share_execution_cache_with_payload_builder` so builds actually receive the shared
//! cache; without a shared cache no prewarming runs regardless of configuration.
//! `--builder.prewarm-workers` budgets an additional pool; account for the engine's
//! existing prewarming workers when sizing total database read concurrency.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
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
    /// Number of IO worker threads in the shared pool. Each worker owns its own
    /// [`CachedStateProvider`] and cache handle for the duration of one job.
    pub worker_count: usize,
    /// Bounded lookahead: transactions scanned ahead of the build loop (the initial
    /// scheduling burst, with one further advance per consumed candidate).
    pub lookahead: usize,
    /// Maximum distinct state keys scheduled per build; once reached the scheduler
    /// saturates and stops advancing the lookahead cursor.
    pub key_cap: usize,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self { enabled: false, worker_count: 2, lookahead: 64, key_cap: 4096 }
    }
}

/// A warmable state location read by a declared validity predicate.
///
/// Context predicates ([`ValidityPredicate::BlockNumber`],
/// [`ValidityPredicate::FlashblockIndex`]) read no state and produce no keys.
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

    /// Returns the warmable state locations read by a predicate batch, in declaration order.
    pub fn for_predicates(predicates: &[ValidityPredicate]) -> impl Iterator<Item = Self> + '_ {
        predicates.iter().filter_map(Self::from_predicate)
    }

    /// Warms this key through a cache-filling provider.
    ///
    /// The storage slot is keyed exactly like the build loop's revm read
    /// (`B256::new(slot.to_be_bytes())`), so warmed entries convert the build's
    /// first-touch reads into shared-cache hits. Read errors are logged and counted;
    /// they never fail the build, and warm results are never used for correctness
    /// decisions.
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

/// A unit of prewarm work dispatched to an IO worker.
///
/// This is the extension seam for future warming modes: new variants (for example full
/// transaction simulation) slot into the same bounded queue and worker loop without
/// changing the scheduling, cancellation, or cache-release structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WarmJob {
    /// Warm the state read by one declared predicate key.
    Key(WarmKey),
}

/// Bounded work queue shared between the build-thread scheduler and prewarm workers.
///
/// Pushes never block on IO; they take a short, bounded mutex critical section and drop
/// the job when the queue is full or closed. Popping blocks until a job is available or
/// the queue is closed; closing cancels every queued job so workers release their cache
/// handles promptly.
#[derive(Debug)]
pub struct KeyQueue {
    state: Mutex<KeyQueueState>,
    available: Condvar,
}

/// Guarded state of a [`KeyQueue`].
#[derive(Debug)]
pub struct KeyQueueState {
    /// Queued jobs awaiting a worker.
    pub jobs: VecDeque<WarmJob>,
    /// Maximum queued jobs.
    pub capacity: usize,
    /// Set once the queue is closed; closed queues accept and yield nothing.
    pub closed: bool,
}

impl KeyQueue {
    /// Creates a queue that buffers at most `capacity` jobs.
    pub const fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(KeyQueueState { jobs: VecDeque::new(), capacity, closed: false }),
            available: Condvar::new(),
        }
    }

    /// Enqueues a job without blocking. Returns `false` when the queue is full or closed.
    pub fn push(&self, job: WarmJob) -> bool {
        let mut state = self.state.lock().expect("prewarm queue mutex poisoned");
        if state.closed || state.jobs.len() >= state.capacity {
            return false;
        }
        state.jobs.push_back(job);
        drop(state);
        self.available.notify_one();
        true
    }

    /// Waits for the next job. Returns `None` once the queue is closed; queued jobs are
    /// dropped on close, so a closed queue yields nothing further.
    pub fn pop(&self) -> Option<WarmJob> {
        let mut state = self.state.lock().expect("prewarm queue mutex poisoned");
        loop {
            if let Some(job) = state.jobs.pop_front() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            state = self.available.wait(state).expect("prewarm queue mutex poisoned");
        }
    }

    /// Closes the queue, dropping all queued work and waking every waiting worker.
    /// Idempotent.
    pub fn close(&self) {
        let mut state = self.state.lock().expect("prewarm queue mutex poisoned");
        state.closed = true;
        state.jobs.clear();
        drop(state);
        self.available.notify_all();
    }

    /// Returns whether the queue is closed.
    pub fn is_closed(&self) -> bool {
        self.state.lock().expect("prewarm queue mutex poisoned").closed
    }

    /// Returns the number of jobs queued and not yet taken by a worker.
    pub fn len(&self) -> usize {
        self.state.lock().expect("prewarm queue mutex poisoned").jobs.len()
    }

    /// Returns whether no work is queued.
    pub fn is_empty(&self) -> bool {
        self.state.lock().expect("prewarm queue mutex poisoned").jobs.is_empty()
    }
}

/// Per-build scheduler: extracts, deduplicates, and caps the predicate-state keys
/// dispatched to prewarm workers.
pub struct PrewarmScheduler {
    /// Bounded work queue consumed by workers.
    pub queue: KeyQueue,
    /// Distinct keys already scheduled for this build.
    pub scheduled: Mutex<HashSet<WarmKey>>,
    /// Bounded lookahead in transactions (the initial burst size).
    pub lookahead: usize,
    /// Maximum distinct keys per build.
    pub key_cap: usize,
    /// Set when the key cap is reached or the queue refuses work; stops cursor advances.
    pub saturated: AtomicBool,
}

impl std::fmt::Debug for PrewarmScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrewarmScheduler")
            .field("lookahead", &self.lookahead)
            .field("key_cap", &self.key_cap)
            .field("saturated", &self.saturated.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl PrewarmScheduler {
    /// Creates a scheduler for one build.
    pub fn new(config: &PrewarmConfig) -> Self {
        // The queue buffer is bounded by the per-build distinct-key budget: the scheduler
        // never schedules more than `key_cap` keys, so the outstanding set is bounded the
        // same way.
        Self {
            queue: KeyQueue::new(config.key_cap.max(1)),
            scheduled: Mutex::new(HashSet::default()),
            lookahead: config.lookahead,
            key_cap: config.key_cap,
            saturated: AtomicBool::new(false),
        }
    }

    /// Returns the bounded lookahead in transactions.
    pub const fn lookahead(&self) -> usize {
        self.lookahead
    }

    /// Returns whether no further work will be accepted for this build.
    pub fn is_saturated(&self) -> bool {
        self.saturated.load(Ordering::Relaxed) || self.queue.is_closed()
    }

    /// Returns the number of keys queued and not yet taken by a worker.
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Closes the queue, cancelling queued work. Called when the build's [`PrewarmJob`]
    /// is dropped.
    pub fn close(&self) {
        self.queue.close();
    }

    /// Schedules one warmable key, deduplicating against keys already scheduled for this
    /// build.
    ///
    /// Returns `true` when the key is accounted as scheduled — including duplicates,
    /// which are counted as deduplicated. Returns `false` when the key was not scheduled
    /// (the queue refused it, or the scheduler is saturated). Never blocks.
    pub fn schedule_key(&self, key: WarmKey) -> bool {
        if self.is_saturated() {
            return false;
        }
        let mut scheduled = self.scheduled.lock().expect("prewarm scheduler mutex poisoned");
        if scheduled.len() >= self.key_cap {
            self.saturated.store(true, Ordering::Relaxed);
            PrewarmMetrics::keys_dropped_key_cap_total().increment(1);
            return false;
        }
        if !scheduled.insert(key) {
            drop(scheduled);
            PrewarmMetrics::keys_deduped_total().increment(1);
            return true;
        }
        drop(scheduled);
        if self.queue.push(WarmJob::Key(key)) {
            PrewarmMetrics::keys_scheduled_total().increment(1);
            true
        } else {
            // Queue closed or full: drop the key and stop advancing the cursor for this
            // build. Never block the build loop.
            self.scheduled.lock().expect("prewarm scheduler mutex poisoned").remove(&key);
            self.saturated.store(true, Ordering::Relaxed);
            PrewarmMetrics::keys_dropped_full_total().increment(1);
            false
        }
    }

    /// Schedules the declared predicate state of one lookahead transaction.
    ///
    /// Stops at the first refused key; balance and storage keys are deduplicated per
    /// build, and context predicates (block number, flashblock index) read no state.
    pub fn schedule_transaction<T: BasePooledTx>(&self, transaction: &T) {
        PrewarmMetrics::transactions_scanned_total().increment(1);
        for key in WarmKey::for_predicates(transaction.validity_predicates()) {
            if !self.schedule_key(key) {
                return;
            }
        }
    }
}

/// Signal that every dispatched worker of one [`PrewarmJob`] has exited the job.
#[derive(Debug, Default)]
pub struct JobCompletion {
    /// Workers still working on the job.
    pub remaining: Mutex<usize>,
    /// Woken whenever a worker exits the job.
    pub done: Condvar,
}

impl JobCompletion {
    /// Creates a completion signal for `workers` dispatched workers.
    pub const fn new(workers: usize) -> Self {
        Self { remaining: Mutex::new(workers), done: Condvar::new() }
    }

    /// Records that one worker exited the job.
    pub fn finish_one(&self) {
        let mut remaining = self.remaining.lock().expect("prewarm completion mutex poisoned");
        *remaining = remaining.saturating_sub(1);
        drop(remaining);
        self.done.notify_all();
    }

    /// Waits until every dispatched worker exited the job.
    pub fn wait(&self) {
        let mut remaining = self.remaining.lock().expect("prewarm completion mutex poisoned");
        while *remaining > 0 {
            remaining = self.done.wait(remaining).expect("prewarm completion mutex poisoned");
        }
    }
}

/// Releases a worker reservation and signals completion, including on unwind.
#[derive(Debug)]
pub struct WorkerLease {
    /// Whether this worker is reserved by a build.
    pub busy: Arc<AtomicBool>,
    /// Completion accounting for that build.
    pub completion: Arc<JobCompletion>,
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
        self.completion.finish_one();
    }
}

/// One job executed by one pool worker.
///
/// Workers construct their own provider from `provider_factory` inside their thread (it
/// is `!Sync`, so providers are never shared), warm the job's queued keys through the
/// job's exact `cache`, and drop both before taking the next job so the engine's
/// cache-advancement gating (`usage_count == 1`) is not blocked between builds.
pub struct WorkerJob {
    /// Opens the state provider for the job's exact parent state. One boxed factory per
    /// worker job.
    pub provider_factory: Box<dyn Fn() -> ProviderResult<StateProviderBox> + Send>,
    /// The build's shared execution cache handle for this worker.
    pub cache: ExecutionCache,
    /// The job's scheduler queue.
    pub scheduler: Arc<PrewarmScheduler>,
    /// Released after the job's provider and cache are dropped.
    pub lease: WorkerLease,
}

impl std::fmt::Debug for WorkerJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerJob").field("scheduler", &self.scheduler).finish_non_exhaustive()
    }
}

impl WorkerJob {
    /// Runs the job to completion: opens the provider, warms queued keys until the queue
    /// closes, then releases the provider and cache handle.
    pub fn run(self) {
        if self.scheduler.queue.is_closed() {
            return;
        }
        let provider = match (self.provider_factory)() {
            Ok(state_provider) => CachedStateProvider::new_prewarm(state_provider, self.cache),
            Err(error) => {
                PrewarmMetrics::provider_open_errors_total().increment(1);
                warn!(target: TARGET, error = %error, "failed to open parent state provider for prewarm worker");
                return;
            }
        };
        while let Some(job) = self.scheduler.queue.pop() {
            match job {
                WarmJob::Key(key) => key.warm(&provider),
            }
        }
        // Dropping the provider releases this worker's shared-cache handle before the
        // worker waits for its next job.
        drop(provider);
    }
}

/// Builder-owned pool of bounded prewarm IO workers, shared across all of the builder's
/// builds.
///
/// Threads are spawned once at pool creation (off the hot path) and reused across
/// builds, so concurrent or rapidly cancelled builds cannot accumulate threads: the
/// total is fixed at `worker_count`. Workers hold no cache handles between jobs. When a
/// worker is busy with an earlier build, a new job is dispatched to the remaining
/// workers; a build whose dispatch finds no idle worker skips prewarming (bounded
/// degradation, counted). Dropping the pool detaches its workers without waiting for
/// IO; they exit after finishing any in-flight read.
pub struct PrewarmWorkerPool {
    /// Configuration the pool and its schedulers are sized from.
    pub config: PrewarmConfig,
    /// One bounded mailbox per worker; `try_send` dispatch is nonblocking.
    pub workers: Vec<(SyncSender<WorkerJob>, Arc<AtomicBool>)>,
}

impl std::fmt::Debug for PrewarmWorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrewarmWorkerPool")
            .field("config", &self.config)
            .field("workers", &self.workers.len())
            .finish_non_exhaustive()
    }
}

impl PrewarmWorkerPool {
    /// Creates the pool, spawning `config.worker_count` worker threads once. Disabled
    /// configurations spawn nothing.
    pub fn new(config: &PrewarmConfig) -> Self {
        let mut workers = Vec::new();
        if config.enabled {
            for index in 0..config.worker_count {
                // Bounded mailbox: dispatch is nonblocking (`try_send`), so a busy
                // worker never blocks the build thread.
                let (sender, receiver) = sync_channel(1);
                match thread::Builder::new()
                    .name(format!("prewarm-worker-{index}"))
                    .spawn(move || Self::worker_loop(receiver))
                {
                    Ok(handle) => {
                        // Dropping the handle detaches the worker: pool shutdown never
                        // waits for IO. Workers exit on their own once the pool (and its
                        // mailbox sender) is dropped.
                        drop(handle);
                        workers.push((sender, Arc::new(AtomicBool::new(false))));
                    }
                    Err(error) => {
                        PrewarmMetrics::worker_spawn_errors_total().increment(1);
                        warn!(target: TARGET, error = %error, worker = index, "failed to spawn prewarm worker");
                    }
                }
            }
        }
        Self { config: *config, workers }
    }

    /// Returns the number of IO workers in the pool.
    pub const fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Starts a prewarm job for one build, or returns `None` when prewarming is
    /// disabled or no worker took the job.
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
        let completion = Arc::new(JobCompletion::new(self.workers.len()));
        let mut dispatched = 0;
        for (worker, busy) in &self.workers {
            if busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                completion.finish_one();
                PrewarmMetrics::worker_busy_skips_total().increment(1);
                continue;
            }
            let job = WorkerJob {
                provider_factory: Box::new(provider_factory.clone()),
                cache: cache.clone(),
                scheduler: Arc::clone(&scheduler),
                lease: WorkerLease { busy: Arc::clone(busy), completion: Arc::clone(&completion) },
            };
            match worker.try_send(job) {
                Ok(()) => dispatched += 1,
                Err(TrySendError::Full(_)) => {
                    PrewarmMetrics::worker_busy_skips_total().increment(1);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // A terminated worker is unavailable, not temporarily busy.
                    PrewarmMetrics::worker_disconnected_total().increment(1);
                }
            }
        }
        if dispatched == 0 {
            PrewarmMetrics::jobs_skipped_busy_total().increment(1);
            return None;
        }
        Some(PrewarmJob { scheduler, completion })
    }

    /// Worker body: takes jobs from its mailbox, runs each to completion, and exits when
    /// the pool is dropped (its mailbox sender is gone).
    pub fn worker_loop(jobs: Receiver<WorkerJob>) {
        while let Ok(job) = jobs.recv() {
            job.run();
        }
    }
}

/// One payload build's prewarm job.
///
/// Dropping the job closes the scheduler queue — cancelling queued work and waking
/// workers — without waiting for worker IO. Each worker finishes at most one in-flight
/// blocking read and then releases its cache handle; [`JobCompletion`] tracks when every
/// dispatched worker exited.
pub struct PrewarmJob {
    /// The build's scheduler, shared with the build's lookahead adapters.
    pub scheduler: Arc<PrewarmScheduler>,
    /// Completion signal for the dispatched workers.
    pub completion: Arc<JobCompletion>,
}

impl std::fmt::Debug for PrewarmJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrewarmJob").field("scheduler", &self.scheduler).finish_non_exhaustive()
    }
}

impl PrewarmJob {
    /// Returns the scheduler that build-side lookahead adapters schedule keys through.
    pub const fn scheduler(&self) -> &Arc<PrewarmScheduler> {
        &self.scheduler
    }

    /// Cancels remaining queued work and waits for all dispatched workers to exit the
    /// job. Never called on the build path; used by tests and graceful shutdown.
    pub fn join(self) {
        self.scheduler.close();
        self.completion.wait();
    }
}

impl Drop for PrewarmJob {
    fn drop(&mut self) {
        // Cancels queued work and wakes workers; never blocks the build thread. Workers
        // finish at most one in-flight read and then release their cache handles.
        self.scheduler.close();
    }
}

/// Payload-transaction adapter that schedules bounded lookahead predicate-state warming
/// while the build loop consumes candidates.
///
/// The main transaction iterator is owned and delegated to untouched, so the build's
/// parking lifecycle and dynamic-inclusion semantics are exactly those of the inner
/// adapter. The independent lookahead cursor is opened with the same attributes as the
/// main iterator and is only ever advanced (read-only).
pub struct PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    /// The build's main transaction iterator, delegated to unchanged.
    pub inner: I,
    /// Independent read-only lookahead cursor over the same pool attributes. `None` when
    /// prewarming is inactive or the cursor is exhausted/saturated.
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
            .field("has_scheduler", &self.prewarm.is_some())
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

    /// Wraps an existing lookahead cursor. Schedules the initial bounded burst.
    pub fn with_cursor(
        inner: I,
        cursor: Option<Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>>,
        prewarm: Option<Arc<PrewarmScheduler>>,
    ) -> Self {
        let mut adapter = Self { inner, cursor, prewarm };
        let initial = adapter.prewarm.as_ref().map_or(0, |scheduler| scheduler.lookahead());
        adapter.advance_lookahead(initial);
        adapter
    }

    /// Advances the lookahead cursor by at most `budget` transactions, scheduling each
    /// transaction's declared predicate state. Stops when the scheduler saturates or the
    /// cursor is exhausted. Never blocks.
    pub fn advance_lookahead(&mut self, budget: usize) {
        let Some(scheduler) = self.prewarm.as_ref() else { return };
        for _ in 0..budget {
            if scheduler.is_saturated() {
                self.cursor = None;
                return;
            }
            let Some(transaction) = self.cursor.as_mut().and_then(Iterator::next) else {
                self.cursor = None;
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
        // One lookahead advance per consumed candidate keeps the schedule bounded.
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

    use alloy_consensus::{SignableTransaction, Transaction, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use base_common_consensus::BasePooledTransaction as ConsensusPooledTransaction;
    use base_execution_txpool::{BasePooledTransaction, ValidityOperator};
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;
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

    #[test]
    fn warm_keys_come_from_state_predicates_only() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(3);
        let predicates = [
            balance_predicate(address),
            storage_predicate(address, slot),
            ValidityPredicate::BlockNumber {
                op: base_execution_txpool::ValidityOperator::Equal,
                value: U256::from(10),
            },
            ValidityPredicate::FlashblockIndex {
                op: base_execution_txpool::ValidityOperator::Equal,
                value: U256::from(1),
            },
        ];

        let keys = WarmKey::for_predicates(&predicates).collect::<Vec<_>>();
        assert_eq!(keys, vec![WarmKey::Balance(address), WarmKey::Storage(address, slot)]);
        assert_eq!(WarmKey::from_predicate(&predicates[2]), None);
    }

    #[test]
    fn queue_push_is_bounded_and_nonblocking() {
        let queue = KeyQueue::new(2);
        assert!(queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(1)))));
        assert!(queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(2)))));
        assert_eq!(queue.len(), 2);
        // Full queue drops instead of blocking; a closed queue refuses everything.
        assert!(!queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(3)))));
        queue.close();
        assert!(queue.is_closed());
        assert!(!queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(4)))));
    }

    #[test]
    fn queue_close_wakes_blocked_pop_and_drops_queued_work() {
        let queue = Arc::new(KeyQueue::new(4));
        queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(1))));
        queue.push(WarmJob::Key(WarmKey::Balance(Address::with_last_byte(2))));

        let popped = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handle = {
            let queue = Arc::clone(&queue);
            let popped = Arc::clone(&popped);
            thread::spawn(move || {
                while let Some(job) = queue.pop() {
                    popped.lock().unwrap().push(job);
                }
            })
        };
        assert!(wait_until(|| popped.lock().unwrap().len() == 2, Duration::from_secs(5)));
        queue.close();
        handle.join().expect("blocked pop must wake on close");

        assert_eq!(popped.lock().unwrap().len(), 2);
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn scheduler_dedups_and_saturates_at_key_cap() {
        let address = Address::with_last_byte(1);
        let scheduler = PrewarmScheduler::new(&enabled_config(1, 8, 2));

        assert!(scheduler.schedule_key(WarmKey::Balance(address)));
        assert!(scheduler.schedule_key(WarmKey::Balance(address)), "duplicates count as scheduled");
        assert_eq!(scheduler.queued_len(), 1, "duplicate key must not be queued twice");

        assert!(scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(2))));
        assert!(!scheduler.schedule_key(WarmKey::Balance(Address::with_last_byte(3))));
        assert!(scheduler.is_saturated(), "key cap must saturate the scheduler");
        assert_eq!(scheduler.queued_len(), 2);
    }

    #[test]
    fn disabled_pool_spawns_no_workers_and_starts_no_jobs() {
        let pool = PrewarmWorkerPool::new(&PrewarmConfig::default());
        assert_eq!(pool.worker_count(), 0);
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
    fn disconnected_worker_is_counted_and_releases_job() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        let busy = Arc::new(AtomicBool::new(false));
        let pool = PrewarmWorkerPool {
            config: enabled_config(1, 8, 64),
            workers: vec![(sender, Arc::clone(&busy))],
        };
        let cache = reth_execution_cache::SavedCache::new(B256::ZERO, ExecutionCache::new(1000));
        metrics::with_local_recorder(&recorder, || {
            assert!(
                pool.try_start_job(
                    || panic!("a disconnected worker must not open a provider"),
                    cache.cache().clone(),
                )
                .is_none()
            );
        });
        assert!(!busy.load(Ordering::Acquire));
        assert!(cache.is_available());
        let output = handle.render();
        assert!(
            output.lines().any(|line| line == "base_payload_prewarm_worker_disconnected_total 1")
        );
        assert!(
            !output.lines().any(|line| line == "base_payload_prewarm_worker_busy_skips_total 1")
        );
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
    fn busy_workers_skip_new_jobs_and_partial_dispatch_completes() {
        let pool = PrewarmWorkerPool::new(&enabled_config(2, 8, 64));
        let opened = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(std::sync::Barrier::new(2));
        let first = pool
            .try_start_job(
                {
                    let opened = Arc::clone(&opened);
                    let release = Arc::clone(&release);
                    move || {
                        if opened.fetch_add(1, Ordering::Relaxed) == 0 {
                            return Err(
                                reth_storage_api::errors::ProviderError::FinalizedBlockNotFound,
                            );
                        }
                        release.wait();
                        Ok(Box::new(MockEthProvider::default()) as StateProviderBox)
                    }
                },
                ExecutionCache::new(1000),
            )
            .unwrap();
        assert!(wait_until(
            || opened.load(Ordering::Relaxed) == 2
                && *first.completion.remaining.lock().unwrap() == 1,
            Duration::from_secs(5)
        ));
        let completion = Arc::clone(&first.completion);
        drop(first);

        // Only the worker whose first job failed is available. The blocked worker
        // must not retain a queued job (and its different-parent cache).
        let cache =
            reth_execution_cache::SavedCache::new(B256::repeat_byte(2), ExecutionCache::new(1000));
        let second = pool
            .try_start_job(
                || Ok(Box::new(MockEthProvider::default()) as StateProviderBox),
                cache.cache().clone(),
            )
            .unwrap();
        let second_done = Arc::clone(&second.completion);
        drop(second);
        assert!(wait_until(|| *second_done.remaining.lock().unwrap() == 0, Duration::from_secs(5)));
        assert!(cache.is_available());
        release.wait();
        completion.wait();
    }

    #[test]
    fn prewarm_fills_shared_cache_and_eliminates_backend_reads() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(9);
        warmable_account(&mock, address);

        let (pool, cache, factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &mock, Duration::ZERO);
        let job = pool.try_start_job(factory, cache.clone()).expect("worker must take the job");
        assert!(job.scheduler().schedule_key(WarmKey::Balance(address)));
        assert!(job.scheduler().schedule_key(WarmKey::Storage(address, U256::from(7))));
        assert!(wait_until(
            || slots_cached(&cache, address, std::iter::once(7)),
            Duration::from_secs(5)
        ));
        job.join();

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
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(saved.is_available());
    }

    #[test]
    fn prewarm_tolerates_missing_parent_state() {
        let (pool, cache, _factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &MockEthProvider::default(), Duration::ZERO);
        let job = pool
            .try_start_job(
                || Err(reth_storage_api::errors::ProviderError::FinalizedBlockNotFound),
                cache.clone(),
            )
            .expect("worker must take the job");
        job.join();
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(saved.is_available(), "worker must release the cache after the error");
    }

    #[test]
    fn prewarm_cancellation_drops_queued_work_and_releases_cache() {
        let mock = MockEthProvider::default();
        let address = Address::with_last_byte(5);
        warmable_account(&mock, address);
        let delay = Duration::from_millis(250);
        let (pool, cache, factory, _account_reads, storage_reads) =
            test_pool(&enabled_config(1, 8, 64), &mock, delay);
        let job = pool.try_start_job(factory, cache.clone()).expect("worker must take the job");
        for index in 0..3 {
            assert!(job.scheduler().schedule_key(WarmKey::Storage(address, U256::from(index))));
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
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);
        assert!(wait_until(|| saved.is_available(), Duration::from_secs(5)));
        assert_eq!(
            storage_reads.load(Ordering::Relaxed),
            1,
            "queued work must be cancelled, not warmed"
        );
    }

    #[test]
    fn prewarm_thread_bound_holds_across_dropped_jobs_with_blocked_io() {
        let (pool, cache, _factory, _account_reads, _storage_reads) =
            test_pool(&enabled_config(2, 8, 64), &MockEthProvider::default(), Duration::ZERO);
        assert_eq!(pool.worker_count(), 2);

        // A factory whose provider open blocks: each dispatch ties up workers while the
        // build drops its job immediately, the rapid-cancellation pattern.
        let blocked = || {
            std::thread::sleep(Duration::from_millis(150));
            Ok(Box::new(DelayStateProvider::new(MockEthProvider::default(), Duration::ZERO))
                as StateProviderBox)
        };
        for _ in 0..6 {
            if let Some(job) = pool.try_start_job(blocked, cache.clone()) {
                drop(job);
            }
        }
        let saved = reth_execution_cache::SavedCache::new(B256::ZERO, cache);

        // Every blocked open finishes, every worker releases its cache, and the pool
        // still serves a new build with its fixed threads (no accumulation).
        assert!(wait_until(|| saved.is_available(), Duration::from_secs(10)));
        let final_job = pool
            .try_start_job(
                || {
                    Ok(
                        Box::new(DelayStateProvider::new(
                            MockEthProvider::default(),
                            Duration::ZERO,
                        )) as StateProviderBox,
                    )
                },
                saved.cache().clone(),
            )
            .expect("pool must still accept jobs after repeated cancellations");
        final_job.scheduler().schedule_key(WarmKey::Balance(Address::with_last_byte(1)));
        final_job.join();
        assert!(saved.is_available());
    }

    /// Vec-backed lookahead cursor double.
    struct StaticCursor {
        transactions: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    }

    impl StaticCursor {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self { transactions: transactions.into() }
        }
    }

    impl Iterator for StaticCursor {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.pop_front()
        }
    }

    impl BestTransactions for StaticCursor {
        fn mark_invalid(&mut self, _transaction: &Self::Item, _kind: InvalidPoolTransactionError) {}

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    /// Inner adapter double recording the parking lifecycle calls it receives.
    struct SpyInner {
        transactions: VecDeque<BasePooledTransaction>,
        parked: usize,
        committed: usize,
        promoted: Vec<TxHash>,
        discarded: Vec<TxHash>,
        invalidated: usize,
    }

    impl SpyInner {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self {
                transactions: transactions
                    .into_iter()
                    .map(|transaction| transaction.transaction.clone())
                    .collect(),
                parked: 0,
                committed: 0,
                promoted: Vec::new(),
                discarded: Vec::new(),
                invalidated: 0,
            }
        }
    }

    impl PayloadTransactions for SpyInner {
        type Transaction = BasePooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            self.transactions.pop_front()
        }

        fn mark_invalid(&mut self, _sender: Address, _nonce: u64) {
            self.invalidated += 1;
        }
    }

    impl ParkablePayloadTransactions for SpyInner {
        fn park_current(&mut self) -> bool {
            self.parked += 1;
            true
        }

        fn mark_current_committed(&mut self) {
            self.committed += 1;
        }

        fn promote(&mut self, transaction_hash: TxHash) -> bool {
            self.promoted.push(transaction_hash);
            true
        }

        fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
            self.discarded.push(transaction_hash);
            true
        }
    }

    #[test]
    fn adapter_schedules_initial_burst_then_one_advance_per_candidate() {
        let senders: Vec<Address> =
            (0..4).map(|index| Address::with_last_byte(index + 1)).collect();
        let transactions: Vec<_> = senders
            .iter()
            .map(|sender| {
                validity_transaction(
                    *sender,
                    vec![balance_predicate(*sender), storage_predicate(*sender, U256::from(7))],
                )
            })
            .collect();

        let scheduler = Arc::new(PrewarmScheduler::new(&enabled_config(1, 2, 16)));
        let mut adapter =
            PrewarmingBestTransactions::<SpyInner, BasePooledTransaction>::with_cursor(
                SpyInner::new(transactions.clone()),
                Some(Box::new(StaticCursor::new(transactions))),
                Some(Arc::clone(&scheduler)),
            );

        // Initial bounded burst: lookahead transactions' keys are queued before the loop.
        assert_eq!(scheduler.queued_len(), 4, "two lookahead transactions, two keys each");

        // Each consumed candidate schedules exactly one further lookahead transaction.
        adapter.next(()).expect("candidate");
        assert_eq!(scheduler.queued_len(), 6);

        // The parking lifecycle is delegated to the inner adapter untouched.
        assert!(adapter.park_current());
        assert_eq!(adapter.inner.parked, 1);

        let second = adapter.next(()).expect("candidate");
        assert_eq!(scheduler.queued_len(), 8);

        adapter.mark_current_committed();
        assert_eq!(adapter.inner.committed, 1);

        let hash = *second.hash();
        assert!(adapter.promote(hash));
        assert!(adapter.discard_parked(hash));
        assert_eq!(adapter.inner.promoted, vec![hash]);
        assert_eq!(adapter.inner.discarded, vec![hash]);

        // The lookahead cursor stops at the bounded schedule: exhausting it schedules no
        // further keys.
        assert!(adapter.next(()).is_some());
        assert!(adapter.next(()).is_some());
        assert!(adapter.next(()).is_none());
        assert_eq!(scheduler.queued_len(), 8, "cursor exhausted: no further scheduling");

        adapter.mark_invalid(second.sender(), second.nonce());
        assert_eq!(adapter.inner.invalidated, 1);
    }
}
