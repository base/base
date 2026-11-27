//! Read-only prefetch of storage-trie pages for contracts a block writes heavily.
//!
//! The serial state root that seals a block is the first reader of the storage-trie branch
//! nodes under every changed slot: execution reads slot values, not trie nodes. When a block
//! changes hundreds of slots in one large contract, updated or created, the root takes the
//! resulting page faults one at a time. While the block executes, this module reads the same pages ahead on a
//! small pool of threads, so the root finds them in memory.
//!
//! The prefetch never changes what the block contains or the root it computes: a task builds a
//! storage proof for a chunk of slots on the parent state and drops it. The execution thread
//! never waits on the pool. A full queue drops the chunk and counts it. Ending the build marks its
//! queued chunks discarded, so workers skip them unread, and hands idle state providers to a
//! release thread; a chunk already being read finishes and then releases its provider. The
//! node's graceful shutdown stops the threads, so none reads the database as the process exits.

use std::{
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use alloy_primitives::{
    Address, B256, keccak256,
    map::{AddressMap, B256Map, B256Set},
};
use crossbeam_channel::{Sender, TryRecvError, select};
use parking_lot::Mutex;
use reth_storage_api::{StateProofProvider, StateProviderFactory, errors::ProviderError};
use reth_trie_common::{MultiProofTargetsV2, ProofV2Target};
use revm::{database::OnStateHook, state::EvmState};
use tracing::{debug, warn};

use crate::StoragePrefetchMetrics;

/// A read-only view of the parent state that a prefetch task reads storage-trie pages through.
pub type StoragePrefetchReader = Box<dyn StateProofProvider + Send>;

/// Opens a [`StoragePrefetchReader`] on the parent state of one build.
///
/// A worker calls it the first time it needs a reader for the build and reuses the reader for
/// later chunks of the same build, so the in-memory overlay of unpersisted blocks is built at
/// most once per worker per build, never per chunk.
pub type StoragePrefetchOpener =
    Box<dyn Fn() -> Result<StoragePrefetchReader, ProviderError> + Send + Sync>;

/// Worker threads shared by every build of one payload builder.
///
/// The pool's sizes are constants rather than settings: `--builder.storage-prefetch` is its only
/// flag. The threads exit on [`Self::shutdown`] or when the pool is dropped, even while sessions and
/// hooks it started are still alive. Once every worker has exited and its receiver has
/// disconnected, those sessions and hooks send nothing more and free their own readers.
#[derive(Debug)]
pub struct StoragePrefetchPool {
    sender: Sender<StoragePrefetchJob>,
    releaser: Sender<StoragePrefetchRelease>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Dropped by [`Self::shutdown`] to stop the threads; nothing is ever sent on it.
    stop: Mutex<Option<Sender<()>>>,
}

/// Readers a build released, with the build's state, sent to the pool's release thread.
pub type StoragePrefetchRelease = (Vec<StoragePrefetchReader>, Arc<StoragePrefetchBlockState>);

impl StoragePrefetchPool {
    /// Distinct changed slots, updated or created, that one contract must exceed in one block
    /// before its slots are prefetched.
    ///
    /// Most blocks change a handful of slots per contract, which the root reads quickly. The
    /// root stalls on contracts that change hundreds of slots at once, such as a batched claim
    /// that writes 725. The gate sits well below such writes and well above the handful of slots
    /// an ordinary transaction changes.
    pub const GATE_SLOTS: usize = 32;

    /// Worker threads that read storage-trie pages.
    ///
    /// The root's faults are latency-bound, so more parallel reads cut them further until the
    /// device saturates. In a cold-cache benchmark of a 725-slot write at 0.19 ms per fault,
    /// 16 workers beat 8 at every lead time (30.5 ms against 39.6 ms with no lead), and 32 or 64
    /// gained little more (26.6 and 25.6 ms). The workers sleep in page faults, not on CPU.
    pub const WORKERS: usize = 16;

    /// Most slots one task proves. Small enough to spread a 725-slot write across all
    /// [`Self::WORKERS`] (23 chunks), large enough that a chunk's proof walk shares the
    /// contract's upper trie levels. Not tuned: the benchmarks above used this value.
    pub const CHUNK_SLOTS: usize = 32;

    /// Chunks that may wait for a worker; the hook drops and counts further chunks rather than
    /// wait. 256 chunks hold 8,192 slots, about eleven times a 725-slot batched claim.
    pub const QUEUE_CHUNKS: usize = 256;

    /// Longest the node's shutdown waits in [`Self::shutdown`] for a chunk being read and for
    /// readers being freed. Below Reth's 5 s graceful-shutdown limit; one chunk's proof takes
    /// milliseconds.
    pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

    /// Starts the pool's worker threads, sized by [`Self::WORKERS`] and [`Self::QUEUE_CHUNKS`].
    pub fn spawn() -> std::io::Result<Self> {
        Self::spawn_sized(Self::WORKERS, Self::QUEUE_CHUNKS)
    }

    /// Starts a pool with `workers` threads and room for `queue_chunks` waiting chunks. Tests and
    /// benches use it to stall or overflow a small pool; production uses [`Self::spawn`].
    pub fn spawn_sized(workers: usize, queue_chunks: usize) -> std::io::Result<Self> {
        let (sender, receiver) =
            crossbeam_channel::bounded::<StoragePrefetchJob>(queue_chunks.max(1));
        let (stop, stopped) = crossbeam_channel::bounded::<()>(0);
        let mut threads = Vec::with_capacity(workers.max(1) + 1);
        for index in 0..workers.max(1) {
            let receiver = receiver.clone();
            let stopped = stopped.clone();
            let worker = thread::Builder::new().name(format!("storage-prefetch-{index}"));
            threads.push(worker.spawn(move || {
                loop {
                    select! {
                        recv(receiver) -> job => {
                            let Ok(job) = job else { break };
                            // A stopped pool leaves queued chunks unread.
                            if stopped.try_recv() == Err(TryRecvError::Disconnected) {
                                break;
                            }
                            let state = Arc::clone(&job.state);
                            // A panic inside a provider must not take the worker down with it.
                            if panic::catch_unwind(AssertUnwindSafe(|| job.run())).is_err() {
                                warn!(
                                    target: "payload_builder",
                                    "storage-trie prefetch chunk panicked"
                                );
                                StoragePrefetchMetrics::storage_prefetch_errors_total()
                                    .increment(1);
                                state.errors.fetch_add(1, Ordering::AcqRel);
                            }
                        }
                        recv(stopped) -> _ => break,
                    }
                }
            })?);
        }
        // Parked readers may hold a large in-memory overlay of unpersisted blocks; ending a
        // build hands them here so freeing it stays off the build thread.
        let (releaser, released) = crossbeam_channel::unbounded::<StoragePrefetchRelease>();
        let release = thread::Builder::new().name("storage-prefetch-release".to_string());
        threads.push(release.spawn(move || {
            let free = |(readers, state): StoragePrefetchRelease| {
                let count = readers.len() as u64;
                drop(readers);
                state.live_readers.fetch_sub(count, Ordering::AcqRel);
            };
            loop {
                select! {
                    recv(released) -> release => match release {
                        Ok(release) => free(release),
                        Err(_) => break,
                    },
                    recv(stopped) -> _ => {
                        // Free what ended builds already handed over. Once this thread is
                        // gone, a build that ends later frees its readers itself.
                        released.try_iter().for_each(free);
                        break;
                    }
                }
            }
        })?);
        Ok(Self { sender, releaser, threads: Mutex::new(threads), stop: Mutex::new(Some(stop)) })
    }

    /// Returns how many of the pool's worker and release threads are still running.
    pub fn live_threads(&self) -> usize {
        self.threads.lock().iter().filter(|thread| !thread.is_finished()).count()
    }

    /// Stops the pool's threads and waits up to `timeout` for them to exit, returning whether
    /// they did. A worker finishes the chunk it is reading and leaves queued chunks unread; the
    /// release thread frees the readers already handed to it. Once it returns `true`, every
    /// worker's receiver has disconnected, and a build still running sends no more chunks and
    /// frees its own readers. After a timeout a worker can still hold its receiver, so until that
    /// worker exits a build can still queue chunks and count full-queue drops.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        drop(self.stop.lock().take());
        let deadline = Instant::now() + timeout;
        loop {
            let live = self.live_threads();
            if live == 0 {
                for thread in self.threads.lock().drain(..) {
                    let _ = thread.join();
                }
                return true;
            }
            if Instant::now() >= deadline {
                warn!(
                    target: "payload_builder",
                    live_threads = live,
                    "storage-trie prefetch threads still running at shutdown"
                );
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Starts prefetching for one build on top of `parent`. Each worker opens its own
    /// read-only view with `client.state_by_block_hash(parent)`, the provider the build itself
    /// uses: the in-memory overlay of unpersisted blocks while the parent is in memory.
    pub fn parent_state_session<C>(&self, client: C, parent: B256) -> StoragePrefetchSession
    where
        C: StateProviderFactory + Send + Sync + 'static,
    {
        self.session(Box::new(move || {
            client.state_by_block_hash(parent).map(|state| state as StoragePrefetchReader)
        }))
    }

    /// Starts prefetching for one build whose parent state `opener` reads.
    pub fn session(&self, opener: StoragePrefetchOpener) -> StoragePrefetchSession {
        StoragePrefetchSession {
            state: Arc::new(StoragePrefetchBlockState::new(opener, self.releaser.clone())),
            sender: self.sender.clone(),
            ended: false,
        }
    }
}

/// Prefetching for one build, owned by the build.
///
/// Dropping the session, or calling [`Self::finish`], ends it: queued chunks are discarded
/// unread as workers reach them, and idle readers are released on the pool's release thread.
#[derive(Debug)]
pub struct StoragePrefetchSession {
    state: Arc<StoragePrefetchBlockState>,
    sender: Sender<StoragePrefetchJob>,
    ended: bool,
}

impl StoragePrefetchSession {
    /// Returns the build's shared prefetch state, for its counters.
    pub const fn state(&self) -> &Arc<StoragePrefetchBlockState> {
        &self.state
    }

    /// Returns the state hook that enqueues prefetches for the transactions the build commits.
    pub fn hook(&self) -> StoragePrefetchHook {
        StoragePrefetchHook {
            state: Arc::clone(&self.state),
            sender: self.sender.clone(),
            contracts: AddressMap::default(),
        }
    }

    /// Records that execution ended: dropping the hook afterwards sends each triggered
    /// contract's last partial chunk. A hook dropped without it, on an early return, sends
    /// nothing.
    pub fn end_execution(&self) {
        self.state.execution_ended.store(true, Ordering::Release);
    }

    /// Records that the state root started. Implies [`Self::end_execution`].
    pub fn mark_root_started(&self) {
        self.end_execution();
        self.state.root_started_ns.store(self.state.elapsed_ns(), Ordering::Release);
    }

    /// Ends the session after the block was sealed and reports its prefetch telemetry, logging
    /// `sealed_after`, the time from the end of execution until `builder.finish` returned.
    /// Returns whether the build issued or dropped a prefetch chunk, so the caller can record
    /// the block's state-root duration where it measures it for every block.
    pub fn finish(mut self, block_number: u64, sealed_after: Duration) -> bool {
        self.end(Some((block_number, sealed_after)));
        let stats = self.state.stats();
        stats.chunks_issued > 0 || stats.chunks_dropped > 0
    }

    fn end(&mut self, sealed: Option<(u64, Duration)>) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.state.cancel();
        self.state.report(sealed);
    }
}

impl Drop for StoragePrefetchSession {
    fn drop(&mut self) {
        self.end(None);
    }
}

/// A snapshot of one build's prefetch counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoragePrefetchStats {
    /// Slots in chunks that entered the queue.
    pub slots_issued: u64,
    /// Chunks that entered the queue.
    pub chunks_issued: u64,
    /// Slots in chunks dropped because the queue was full.
    pub slots_dropped: u64,
    /// Chunks dropped because the queue was full.
    pub chunks_dropped: u64,
    /// Chunks a worker started reading.
    pub chunks_started: u64,
    /// Chunks whose proof completed.
    pub chunks_completed: u64,
    /// Queued chunks discarded because the build had ended.
    pub chunks_cancelled: u64,
    /// Chunks that failed to open a reader or to build their proof.
    pub errors: u64,
    /// Readers currently open, whether idle or in use.
    pub live_readers: u64,
}

impl StoragePrefetchStats {
    /// Whether every issued chunk has been read, discarded, or failed.
    pub const fn is_settled(&self) -> bool {
        self.chunks_started + self.chunks_cancelled == self.chunks_issued
            && self.chunks_completed + self.errors >= self.chunks_started
    }
}

/// State one build shares with its queued and running prefetch tasks.
#[derive(derive_more::Debug)]
pub struct StoragePrefetchBlockState {
    #[debug(skip)]
    opener: StoragePrefetchOpener,
    releaser: Sender<StoragePrefetchRelease>,
    #[debug(skip)]
    idle_readers: Mutex<Vec<StoragePrefetchReader>>,
    cancelled: AtomicBool,
    execution_ended: AtomicBool,
    started_at: Instant,
    /// Nanoseconds from `started_at` to the first chunk entering the queue.
    first_issue_ns: AtomicU64,
    /// Nanoseconds from `started_at` to the end of execution; `u64::MAX` until then.
    root_started_ns: AtomicU64,
    first_start_ns: AtomicU64,
    last_done_ns: AtomicU64,
    /// Completion time, in nanoseconds from `started_at`, and slot count of each completed chunk.
    completions: Mutex<Vec<(u64, u64)>>,
    slots_issued: AtomicU64,
    chunks_issued: AtomicU64,
    slots_dropped: AtomicU64,
    chunks_dropped: AtomicU64,
    chunks_started: AtomicU64,
    chunks_completed: AtomicU64,
    chunks_cancelled: AtomicU64,
    errors: AtomicU64,
    live_readers: AtomicU64,
}

impl StoragePrefetchBlockState {
    fn new(opener: StoragePrefetchOpener, releaser: Sender<StoragePrefetchRelease>) -> Self {
        Self {
            opener,
            releaser,
            idle_readers: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            execution_ended: AtomicBool::new(false),
            started_at: Instant::now(),
            first_issue_ns: AtomicU64::new(u64::MAX),
            root_started_ns: AtomicU64::new(u64::MAX),
            first_start_ns: AtomicU64::new(u64::MAX),
            last_done_ns: AtomicU64::new(0),
            completions: Mutex::new(Vec::new()),
            slots_issued: AtomicU64::new(0),
            chunks_issued: AtomicU64::new(0),
            slots_dropped: AtomicU64::new(0),
            chunks_dropped: AtomicU64::new(0),
            chunks_started: AtomicU64::new(0),
            chunks_completed: AtomicU64::new(0),
            chunks_cancelled: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            live_readers: AtomicU64::new(0),
        }
    }

    /// Returns a snapshot of the build's counters.
    pub fn stats(&self) -> StoragePrefetchStats {
        StoragePrefetchStats {
            slots_issued: self.slots_issued.load(Ordering::Acquire),
            chunks_issued: self.chunks_issued.load(Ordering::Acquire),
            slots_dropped: self.slots_dropped.load(Ordering::Acquire),
            chunks_dropped: self.chunks_dropped.load(Ordering::Acquire),
            chunks_started: self.chunks_started.load(Ordering::Acquire),
            chunks_completed: self.chunks_completed.load(Ordering::Acquire),
            chunks_cancelled: self.chunks_cancelled.load(Ordering::Acquire),
            errors: self.errors.load(Ordering::Acquire),
            live_readers: self.live_readers.load(Ordering::Acquire),
        }
    }

    /// Whether the build that owns this state has ended.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Marks the build ended and releases idle readers. Workers check the flag under the same
    /// lock before returning a reader, so no reader is parked after this returns.
    fn cancel(self: &Arc<Self>) {
        let released = {
            let mut idle = self.idle_readers.lock();
            self.cancelled.store(true, Ordering::Release);
            std::mem::take(&mut *idle)
        };
        if released.is_empty() {
            return;
        }
        if let Err(error) = self.releaser.send((released, Arc::clone(self))) {
            // The release thread is gone; free the readers here instead.
            let (released, _) = error.into_inner();
            let count = released.len() as u64;
            drop(released);
            self.live_readers.fetch_sub(count, Ordering::AcqRel);
        }
    }

    fn record_error(&self, error: &ProviderError) {
        debug!(target: "payload_builder", error = %error, "storage-trie prefetch chunk failed");
        StoragePrefetchMetrics::storage_prefetch_errors_total().increment(1);
        self.errors.fetch_add(1, Ordering::AcqRel);
    }

    /// Records per-block metrics and the summary log line once the build has ended.
    fn report(&self, sealed: Option<(u64, Duration)>) {
        let stats = self.stats();
        if stats.chunks_issued == 0 && stats.chunks_dropped == 0 {
            return;
        }
        StoragePrefetchMetrics::storage_prefetch_slots_issued_total().increment(stats.slots_issued);
        StoragePrefetchMetrics::storage_prefetch_chunks_issued_total()
            .increment(stats.chunks_issued);
        StoragePrefetchMetrics::storage_prefetch_slots_dropped_total()
            .increment(stats.slots_dropped);
        StoragePrefetchMetrics::storage_prefetch_chunks_dropped_total()
            .increment(stats.chunks_dropped);

        let root_started = self.root_started_ns.load(Ordering::Acquire);
        let mut slots_before_root = 0;
        let mut chunks_late = 0;
        if root_started != u64::MAX {
            let lead_time = StoragePrefetchMetrics::storage_prefetch_lead_time();
            for &(done, slots) in self.completions.lock().iter() {
                if done <= root_started {
                    lead_time.record(Duration::from_nanos(root_started - done).as_secs_f64());
                    slots_before_root += slots;
                } else {
                    chunks_late += 1;
                }
            }
            StoragePrefetchMetrics::storage_prefetch_slots_before_root()
                .record(slots_before_root as f64);
        }
        // How long before the root the first chunk was queued: the head start the trigger gave.
        let first_issue = self.first_issue_ns.load(Ordering::Acquire);
        let trigger_lead = Duration::from_nanos(root_started.saturating_sub(first_issue));
        let first_start = self.first_start_ns.load(Ordering::Acquire);
        let wall = Duration::from_nanos(
            self.last_done_ns.load(Ordering::Acquire).saturating_sub(first_start),
        );
        if first_start != u64::MAX {
            StoragePrefetchMetrics::storage_prefetch_wall_duration().record(wall.as_secs_f64());
        }

        let not_started = stats.chunks_issued.saturating_sub(stats.chunks_started);
        if let Some((block_number, sealed_after)) = sealed {
            StoragePrefetchMetrics::storage_prefetch_blocks_total().increment(1);
            debug!(
                target: "payload_builder",
                block_number,
                slots_issued = stats.slots_issued,
                slots_dropped = stats.slots_dropped,
                slots_before_root,
                chunks_issued = stats.chunks_issued,
                chunks_completed = stats.chunks_completed,
                chunks_late,
                chunks_not_started = not_started,
                errors = stats.errors,
                trigger_lead = ?trigger_lead,
                prefetch_wall = ?wall,
                sealed_after = ?sealed_after,
                "storage-trie prefetch"
            );
        } else {
            debug!(
                target: "payload_builder",
                slots_issued = stats.slots_issued,
                chunks_not_started = not_started,
                "storage-trie prefetch ended with an unsealed build"
            );
        }
    }
}

/// A chunk of one contract's slots to prove on the parent state.
#[derive(Debug)]
pub struct StoragePrefetchJob {
    state: Arc<StoragePrefetchBlockState>,
    address: Address,
    slots: Vec<B256>,
}

impl StoragePrefetchJob {
    /// Reads the storage-trie pages under the chunk's slots, unless the build has ended.
    pub fn run(self) {
        let state = &self.state;
        if state.is_cancelled() {
            StoragePrefetchMetrics::storage_prefetch_chunks_cancelled_total().increment(1);
            state.chunks_cancelled.fetch_add(1, Ordering::AcqRel);
            return;
        }
        state.chunks_started.fetch_add(1, Ordering::AcqRel);
        state.first_start_ns.fetch_min(state.elapsed_ns(), Ordering::AcqRel);

        let parked = state.idle_readers.lock().pop();
        let lease = match parked {
            Some(reader) => StoragePrefetchLease { reader: Some(reader), state },
            None => match (state.opener)() {
                Ok(reader) => {
                    state.live_readers.fetch_add(1, Ordering::AcqRel);
                    StoragePrefetchLease { reader: Some(reader), state }
                }
                Err(error) => {
                    state.record_error(&error);
                    return;
                }
            },
        };
        // The build may have ended while the reader was opened; do not start a proof for it.
        if state.is_cancelled() {
            drop(lease);
            state.chunks_started.fetch_sub(1, Ordering::AcqRel);
            StoragePrefetchMetrics::storage_prefetch_chunks_cancelled_total().increment(1);
            state.chunks_cancelled.fetch_add(1, Ordering::AcqRel);
            return;
        }
        // The proof walks the storage trie and hashed storage under the chunk's slots; the
        // pages it faults in are what the state root reads next. The proof itself is unused.
        // A V2 multiproof reuses the reader's overlay as is, where a storage multiproof would
        // copy the overlay's hashed state for every chunk.
        let targets = MultiProofTargetsV2 {
            account_targets: Vec::new(),
            storage_targets: B256Map::from_iter([(
                keccak256(self.address),
                self.slots.iter().map(|slot| ProofV2Target::new(keccak256(slot))).collect(),
            )]),
        };
        let result = lease.reader().multiproof_v2(targets, false);
        let done = state.elapsed_ns();
        lease.park();

        state.last_done_ns.fetch_max(done, Ordering::AcqRel);
        match result {
            Ok(_) => {
                StoragePrefetchMetrics::storage_prefetch_chunks_completed_total().increment(1);
                if done > state.root_started_ns.load(Ordering::Acquire) {
                    StoragePrefetchMetrics::storage_prefetch_chunks_late_total().increment(1);
                }
                state.completions.lock().push((done, self.slots.len() as u64));
                state.chunks_completed.fetch_add(1, Ordering::AcqRel);
            }
            Err(error) => state.record_error(&error),
        }
    }
}

/// A reader one chunk holds. Dropping the lease, including in a panic's unwind, releases the
/// reader and its count; [`Self::park`] returns it for the build's next chunk instead.
#[derive(derive_more::Debug)]
pub struct StoragePrefetchLease<'a> {
    #[debug(skip)]
    reader: Option<StoragePrefetchReader>,
    state: &'a StoragePrefetchBlockState,
}

impl StoragePrefetchLease<'_> {
    /// Returns the leased reader.
    pub fn reader(&self) -> &StoragePrefetchReader {
        self.reader.as_ref().expect("a lease holds its reader until parked or dropped")
    }

    /// Parks the reader for the build's next chunk, or releases it if the build has ended. The
    /// check shares the lock `cancel` takes, so no reader is parked after `cancel` returns.
    pub fn park(mut self) {
        let mut idle = self.state.idle_readers.lock();
        if !self.state.is_cancelled() {
            idle.extend(self.reader.take());
        }
    }
}

impl Drop for StoragePrefetchLease<'_> {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            drop(reader);
            self.state.live_readers.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Changed slots of one contract seen so far in the block.
#[derive(Debug, Default)]
pub struct StoragePrefetchContract {
    /// Distinct changed slots, updated or created.
    seen: B256Set,
    /// Slots not yet sent to the pool.
    pending: Vec<B256>,
}

/// State hook that enqueues prefetches for contracts the block writes heavily.
///
/// Counts, per contract and across the block, the distinct changed slots, updated or created: the
/// root walks the trie path of a new leaf as it does of an updated one. A slot written by many
/// transactions counts and is prefetched once. Once a contract's count exceeds the gate, its
/// pending slots go to the pool in chunks of [`StoragePrefetchPool::CHUNK_SLOTS`]; a remainder
/// smaller than a chunk waits for more writes and is sent when the hook is dropped after
/// execution ends, before the state root starts.
#[derive(Debug)]
pub struct StoragePrefetchHook {
    state: Arc<StoragePrefetchBlockState>,
    sender: Sender<StoragePrefetchJob>,
    contracts: AddressMap<StoragePrefetchContract>,
}

impl StoragePrefetchHook {
    /// Counts the changed slots of one commit and enqueues prefetches for contracts above the
    /// gate.
    pub fn observe(&mut self, state: &EvmState) {
        for (address, account) in state {
            let mut changed = account
                .storage
                .iter()
                .filter(|(_, slot)| slot.is_changed())
                .map(|(key, _)| B256::from(*key))
                .peekable();
            if changed.peek().is_none() {
                continue;
            }
            let contract = self.contracts.entry(*address).or_default();
            for key in changed {
                if contract.seen.insert(key) {
                    contract.pending.push(key);
                }
            }
            if contract.seen.len() > StoragePrefetchPool::GATE_SLOTS
                && contract.pending.len() >= StoragePrefetchPool::CHUNK_SLOTS
            {
                let full = contract.pending.len() / StoragePrefetchPool::CHUNK_SLOTS
                    * StoragePrefetchPool::CHUNK_SLOTS;
                Self::enqueue(&self.state, &self.sender, *address, &contract.pending[..full]);
                contract.pending.drain(..full);
            }
        }
    }

    /// Sends `slots` to the pool in chunks, dropping and counting any chunk the full queue
    /// refuses. Once every worker's receiver has disconnected, after a completed shutdown or a
    /// pool drop, it sends nothing more and counts nothing. Never waits.
    fn enqueue(
        state: &Arc<StoragePrefetchBlockState>,
        sender: &Sender<StoragePrefetchJob>,
        address: Address,
        slots: &[B256],
    ) {
        if state.is_cancelled() {
            return;
        }
        for chunk in slots.chunks(StoragePrefetchPool::CHUNK_SLOTS) {
            let count = chunk.len() as u64;
            // Counted before the send so a worker never finishes a chunk the counters lack.
            state.chunks_issued.fetch_add(1, Ordering::AcqRel);
            state.first_issue_ns.fetch_min(state.elapsed_ns(), Ordering::AcqRel);
            state.slots_issued.fetch_add(count, Ordering::AcqRel);
            let job =
                StoragePrefetchJob { state: Arc::clone(state), address, slots: chunk.to_vec() };
            let Err(error) = sender.try_send(job) else { continue };
            state.chunks_issued.fetch_sub(1, Ordering::AcqRel);
            state.slots_issued.fetch_sub(count, Ordering::AcqRel);
            // A stopped pool reads nothing more. Its chunks are neither issued nor dropped, so
            // the dropped counters only ever mean a full queue.
            if error.is_disconnected() {
                return;
            }
            state.chunks_dropped.fetch_add(1, Ordering::AcqRel);
            state.slots_dropped.fetch_add(count, Ordering::AcqRel);
        }
    }
}

impl Drop for StoragePrefetchHook {
    /// Sends each triggered contract's last partial chunk once execution has ended. A build that
    /// returned early sends nothing.
    fn drop(&mut self) {
        if !self.state.execution_ended.load(Ordering::Acquire) {
            return;
        }
        for (address, contract) in &self.contracts {
            if contract.seen.len() > StoragePrefetchPool::GATE_SLOTS && !contract.pending.is_empty()
            {
                Self::enqueue(&self.state, &self.sender, *address, &contract.pending);
            }
        }
    }
}

impl OnStateHook for StoragePrefetchHook {
    fn on_state(&mut self, state: EvmState) {
        self.observe(&state);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
    use crossbeam_channel::Receiver;
    use parking_lot::Mutex;
    use reth_storage_api::{StateProofProvider, errors::ProviderError};
    use reth_trie_common::{
        AccountProof, DecodedMultiProofV2, ExecutionWitnessMode, HashedPostState, MultiProof,
        MultiProofTargets, MultiProofTargetsV2, ProofV2Target, TrieInput,
    };
    use revm::{
        database::OnStateHook,
        state::{Account, EvmState, EvmStorageSlot, TransactionId},
    };

    use super::{
        StoragePrefetchOpener, StoragePrefetchPool, StoragePrefetchReader, StoragePrefetchSession,
    };

    /// Test double for a parent-state reader. Hand-rolled rather than `automock`ed because the
    /// trait comes from reth and the double must block inside a proof until the test releases
    /// it, and count live instances through `Drop`.
    struct GatedReader {
        release: Receiver<()>,
        probe: Arc<ReaderProbe>,
    }

    #[derive(Default)]
    struct ReaderProbe {
        live: AtomicUsize,
        opened: AtomicUsize,
        proofs: AtomicUsize,
        /// Hashed address and hashed slot keys of each proof.
        slots: Mutex<Vec<(B256, Vec<B256>)>>,
    }

    impl Drop for GatedReader {
        fn drop(&mut self) {
            self.probe.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl StateProofProvider for GatedReader {
        fn proof(
            &self,
            _: TrieInput,
            _: Address,
            _: &[B256],
        ) -> Result<AccountProof, ProviderError> {
            Err(ProviderError::UnsupportedProvider)
        }

        fn multiproof(
            &self,
            _: TrieInput,
            _: MultiProofTargets,
        ) -> Result<MultiProof, ProviderError> {
            Err(ProviderError::UnsupportedProvider)
        }

        fn multiproof_v2(
            &self,
            targets: MultiProofTargetsV2,
            _: bool,
        ) -> Result<DecodedMultiProofV2, ProviderError> {
            self.probe.proofs.fetch_add(1, Ordering::SeqCst);
            self.probe.slots.lock().extend(
                targets.storage_targets.into_iter().map(|(address, slots)| {
                    (address, slots.iter().map(ProofV2Target::key).collect())
                }),
            );
            // A dropped sender releases every waiting proof.
            let _ = self.release.recv();
            Ok(DecodedMultiProofV2::default())
        }

        fn witness(
            &self,
            _: TrieInput,
            _: HashedPostState,
            _: ExecutionWitnessMode,
        ) -> Result<Vec<Bytes>, ProviderError> {
            Err(ProviderError::UnsupportedProvider)
        }
    }

    /// Test double whose proof panics, standing in for a provider bug. Hand-rolled for the same
    /// reason as [`GatedReader`]: the trait comes from reth and the double counts live instances.
    struct PanickingReader {
        probe: Arc<ReaderProbe>,
    }

    impl Drop for PanickingReader {
        fn drop(&mut self) {
            self.probe.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl StateProofProvider for PanickingReader {
        fn proof(
            &self,
            _: TrieInput,
            _: Address,
            _: &[B256],
        ) -> Result<AccountProof, ProviderError> {
            Err(ProviderError::UnsupportedProvider)
        }

        fn multiproof(
            &self,
            _: TrieInput,
            _: MultiProofTargets,
        ) -> Result<MultiProof, ProviderError> {
            Err(ProviderError::UnsupportedProvider)
        }

        fn multiproof_v2(
            &self,
            targets: MultiProofTargetsV2,
            _: bool,
        ) -> Result<DecodedMultiProofV2, ProviderError> {
            let _ = targets;
            self.probe.proofs.fetch_add(1, Ordering::SeqCst);
            panic!("provider bug inside a prefetch proof");
        }

        fn witness(
            &self,
            _: TrieInput,
            _: HashedPostState,
            _: ExecutionWitnessMode,
        ) -> Result<Vec<Bytes>, ProviderError> {
            Err(ProviderError::UnsupportedProvider)
        }
    }

    /// Opens gated readers; dropping the returned sender releases every proof.
    fn gated_opener(
        probe: &Arc<ReaderProbe>,
    ) -> (StoragePrefetchOpener, crossbeam_channel::Sender<()>) {
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let probe = Arc::clone(probe);
        let opener: StoragePrefetchOpener = Box::new(move || {
            probe.live.fetch_add(1, Ordering::SeqCst);
            probe.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(GatedReader { release: release_rx.clone(), probe: Arc::clone(&probe) })
                as StoragePrefetchReader)
        });
        (opener, release_tx)
    }

    fn pool(workers: usize, queue_capacity: usize) -> StoragePrefetchPool {
        StoragePrefetchPool::spawn_sized(workers, queue_capacity).expect("spawn pool")
    }

    /// The hashed keys of `slots`, sorted, as proof targets name them.
    fn hashed_slots(slots: std::ops::Range<u64>) -> Vec<B256> {
        let mut hashed: Vec<_> =
            slots.map(|slot| keccak256(B256::from(U256::from(slot)))).collect();
        hashed.sort();
        hashed
    }

    /// One commit that changes `existing` non-zero slots and `new` zero slots of `contract`.
    fn writes(contract: Address, existing: u64, new: u64, first_slot: u64) -> EvmState {
        let mut account = Account::default();
        account.mark_touch();
        for offset in 0..existing + new {
            let original = if offset < existing { U256::from(1) } else { U256::ZERO };
            let mut slot = EvmStorageSlot::new(original, TransactionId::default());
            slot.present_value = U256::from(2);
            account.storage.insert(U256::from(first_slot + offset), slot);
        }
        // A slot the transaction read but did not change.
        account
            .storage
            .insert(U256::MAX, EvmStorageSlot::new(U256::from(7), TransactionId::default()));
        let mut state = EvmState::default();
        state.insert(contract, account);
        state
    }

    fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Runs `hook.on_state` on another thread and fails if it does not return promptly, so a
    /// hook that waited on the pool fails the test instead of hanging it.
    fn commit_without_waiting(session: &StoragePrefetchSession, state: EvmState) {
        session.mark_root_started();
        let mut hook = session.hook();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        thread::spawn(move || {
            hook.on_state(state);
            // Ending execution flushes the remainder; that must not wait either.
            drop(hook);
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(Duration::from_secs(5)).expect("the state hook waited on the pool");
    }

    #[test]
    fn writes_at_or_below_the_gate_issue_nothing() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, _release) = gated_opener(&probe);
        let pool = pool(1, 64);
        let session = pool.session(opener);
        session.mark_root_started();
        // Exactly the gate's worth of changed slots, existing and created together.
        commit_without_waiting(&session, writes(Address::repeat_byte(1), 20, 12, 0));
        assert_eq!(session.state().stats().chunks_issued, 0);
        assert_eq!(session.state().stats().slots_issued, 0);
    }

    #[test]
    fn a_heavy_write_is_issued_in_chunks_of_changed_slots() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        drop(release);
        let pool = pool(2, 64);
        let session = pool.session(opener);
        let contract = Address::repeat_byte(2);
        let mut hook = session.hook();
        hook.on_state(writes(contract, 725, 10, 0));
        session.mark_root_started();
        drop(hook);

        wait_until("every chunk is read", || {
            let stats = session.state().stats();
            stats.chunks_issued > 0 && stats.is_settled()
        });
        let stats = session.state().stats();
        assert_eq!(stats.slots_issued, 735);
        assert_eq!(stats.chunks_issued, 735_u64.div_ceil(32));
        assert_eq!(stats.chunks_dropped, 0);
        let read = probe.slots.lock();
        assert!(
            read.iter()
                .all(|(address, slots)| *address == keccak256(contract) && slots.len() <= 32)
        );
        let mut slots: Vec<_> = read.iter().flat_map(|(_, slots)| slots.clone()).collect();
        slots.sort();
        assert_eq!(slots, hashed_slots(0..735), "exactly the changed slots are prefetched");
    }

    /// Must hold: the root walks the trie path of a created slot just as of an updated one, so
    /// created slots count toward the gate and are prefetched.
    #[test]
    fn created_slots_count_toward_the_gate_and_are_prefetched() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        drop(release);
        let pool = pool(2, 64);
        let session = pool.session(opener);
        let created = Address::repeat_byte(11);
        let mixed = Address::repeat_byte(12);
        let mut hook = session.hook();
        hook.on_state(writes(created, 0, 725, 0));
        hook.on_state(writes(mixed, 16, 17, 0));
        session.mark_root_started();
        drop(hook);

        wait_until("every chunk is read", || session.state().stats().is_settled());
        assert_eq!(session.state().stats().slots_issued, 725 + 33);
        let read = probe.slots.lock();
        let mut created_slots: Vec<_> = read
            .iter()
            .filter(|(address, _)| *address == keccak256(created))
            .flat_map(|(_, slots)| slots.clone())
            .collect();
        created_slots.sort();
        assert_eq!(created_slots, hashed_slots(0..725), "every created slot is prefetched");
    }

    #[test]
    fn a_batch_split_across_transactions_triggers_on_the_running_count() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        drop(release);
        let pool = pool(1, 64);
        let session = pool.session(opener);
        let contract = Address::repeat_byte(3);
        let mut hook = session.hook();
        for tx in 0..10 {
            hook.on_state(writes(contract, 10, 0, tx * 10));
        }
        session.mark_root_started();
        drop(hook);
        wait_until("every chunk is read", || session.state().stats().is_settled());
        assert_eq!(session.state().stats().slots_issued, 100);
    }

    #[test]
    fn repeated_writes_to_the_same_slots_count_once() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        drop(release);
        let pool = pool(1, 64);
        let session = pool.session(opener);
        session.mark_root_started();
        let contract = Address::repeat_byte(10);
        let mut hook = session.hook();
        // A hot pool's reserves: the same three slots in a hundred transactions.
        for _ in 0..100 {
            hook.on_state(writes(contract, 3, 0, 0));
        }
        drop(hook);
        assert_eq!(session.state().stats().chunks_issued, 0);

        // Distinct slots still trigger, and each is prefetched once.
        let mut hook = session.hook();
        for tx in 0..2 {
            hook.on_state(writes(contract, 40, 0, 0));
            hook.on_state(writes(contract, 1, 0, 1_000 + tx));
        }
        drop(hook);
        wait_until("every chunk is read", || session.state().stats().is_settled());
        assert_eq!(session.state().stats().slots_issued, 42);
    }

    /// Must hold: with every worker stuck inside a proof, the execution thread still returns at
    /// once and the overflow is counted rather than waited out.
    #[test]
    fn a_stalled_pool_never_blocks_the_hook_and_counts_drops() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        let pool = pool(1, 1);
        let session = pool.session(opener);

        commit_without_waiting(&session, writes(Address::repeat_byte(6), 725, 0, 0));

        let stats = session.state().stats();
        let chunks = 725_u64.div_ceil(32);
        assert_eq!(stats.chunks_issued + stats.chunks_dropped, chunks);
        // One chunk can be in the worker and one in the queue; the rest are dropped.
        assert!(
            stats.chunks_issued <= 2,
            "issued {} chunks into a stalled pool",
            stats.chunks_issued
        );
        assert!(stats.chunks_dropped >= chunks - 2);
        assert_eq!(stats.slots_issued + stats.slots_dropped, 725);
        drop(release);
    }

    /// Must hold: ending the build discards queued chunks, no chunk starts afterwards, and once
    /// the chunks already being read finish, no reader is left open.
    #[test]
    fn ending_the_build_discards_queued_chunks_and_releases_every_reader() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        let workers = 2;
        let pool = pool(workers, 64);
        let session = pool.session(opener);
        let state = Arc::clone(session.state());
        let mut hook = session.hook();
        hook.on_state(writes(Address::repeat_byte(7), 725, 0, 0));
        session.mark_root_started();
        drop(hook);
        let issued = state.stats().chunks_issued;
        assert_eq!(issued, 725_u64.div_ceil(32));
        wait_until("both workers are inside a proof", || {
            probe.proofs.load(Ordering::SeqCst) == workers
        });

        drop(session);
        assert!(state.is_cancelled());
        drop(release);
        wait_until("the queue drains", || state.stats().is_settled());

        let stats = state.stats();
        assert_eq!(
            probe.proofs.load(Ordering::SeqCst),
            workers,
            "a chunk started after the build ended"
        );
        assert_eq!(stats.chunks_started, workers as u64);
        assert_eq!(stats.chunks_cancelled, issued - workers as u64);
        wait_until("in-flight readers are released", || probe.live.load(Ordering::SeqCst) == 0);
        assert_eq!(state.stats().live_readers, 0);
        assert!(probe.opened.load(Ordering::SeqCst) <= workers);
    }

    /// Must hold: chunks that completed before the session ended park their reader, and ending
    /// the session releases the parked readers at once.
    #[test]
    fn ending_the_build_releases_idle_readers() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        drop(release);
        let pool = pool(2, 64);
        let session = pool.session(opener);
        let state = Arc::clone(session.state());
        let mut hook = session.hook();
        hook.on_state(writes(Address::repeat_byte(8), 200, 0, 0));
        session.mark_root_started();
        drop(hook);
        wait_until("every chunk is read", || state.stats().is_settled());
        assert!(probe.live.load(Ordering::SeqCst) >= 1, "idle readers are reused within a build");

        session.finish(1, Duration::from_millis(1));
        assert!(state.is_cancelled());
        wait_until("parked readers are released", || probe.live.load(Ordering::SeqCst) == 0);
        wait_until("the release is counted", || state.stats().live_readers == 0);
    }

    /// A panic inside a proof is counted as an error, releases the reader it held, and leaves
    /// the worker serving later chunks.
    #[test]
    fn a_panicking_proof_releases_its_reader_and_keeps_the_worker() {
        let probe = Arc::new(ReaderProbe::default());
        let opener_probe = Arc::clone(&probe);
        let opener: StoragePrefetchOpener = Box::new(move || {
            opener_probe.live.fetch_add(1, Ordering::SeqCst);
            opener_probe.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(PanickingReader { probe: Arc::clone(&opener_probe) })
                as StoragePrefetchReader)
        });
        let pool = pool(1, 64);
        let session = pool.session(opener);
        let state = Arc::clone(session.state());
        let mut hook = session.hook();
        hook.on_state(writes(Address::repeat_byte(13), 64, 0, 0));
        session.mark_root_started();
        drop(hook);

        wait_until("both chunks fail", || state.stats().errors == 2);
        assert_eq!(probe.proofs.load(Ordering::SeqCst), 2, "the worker survived the first panic");
        wait_until("the readers are dropped", || probe.live.load(Ordering::SeqCst) == 0);
        assert_eq!(state.stats().live_readers, 0, "a panicking chunk leaked its reader count");
        assert!(state.stats().is_settled());
    }

    /// Ending execution sends each triggered contract's last partial chunk before the state root
    /// starts, so it leads the root instead of racing it.
    #[test]
    fn ending_execution_sends_the_remainder_before_the_root_starts() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        drop(release);
        let pool = pool(1, 64);
        let session = pool.session(opener);
        let mut hook = session.hook();
        hook.on_state(writes(Address::repeat_byte(14), 40, 0, 0));
        assert_eq!(session.state().stats().slots_issued, 32);
        session.end_execution();
        drop(hook);
        assert_eq!(session.state().stats().slots_issued, 40, "sent before the root starts");
        session.mark_root_started();
    }

    #[test]
    fn an_ended_build_issues_nothing() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, _release) = gated_opener(&probe);
        let pool = pool(1, 64);
        let session = pool.session(opener);
        let state = Arc::clone(session.state());
        let mut hook = session.hook();
        drop(session);
        hook.on_state(writes(Address::repeat_byte(9), 725, 0, 0));
        drop(hook);
        assert_eq!(state.stats().chunks_issued, 0);
        assert_eq!(probe.opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shutdown_waits_for_the_chunk_being_read_and_leaves_queued_chunks_unread() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, release) = gated_opener(&probe);
        let pool = pool(1, 64);
        let session = pool.session(opener);
        // Three chunks for the only worker: one blocks inside its proof, two wait in the queue.
        commit_without_waiting(&session, writes(Address::repeat_byte(9), 96, 0, 0));
        wait_until("the first chunk is being read", || probe.proofs.load(Ordering::SeqCst) == 1);

        assert!(!pool.shutdown(Duration::from_millis(50)), "shutdown returned during a read");
        assert_eq!(pool.live_threads(), 1, "the release thread must exit, the worker must not");

        drop(release);
        assert!(pool.shutdown(Duration::from_secs(10)));
        assert_eq!(pool.live_threads(), 0);
        assert_eq!(
            probe.proofs.load(Ordering::SeqCst),
            1,
            "a queued chunk was read after shutdown"
        );
        assert_eq!(session.state().stats().chunks_started, 1);
    }

    /// Must hold: a build still running after the pool stopped counts nothing as dropped, so the
    /// dropped counters only ever mean a full queue, and the block is not reported as prefetched.
    #[test]
    fn a_stopped_pool_counts_no_drops() {
        let probe = Arc::new(ReaderProbe::default());
        let (opener, _release) = gated_opener(&probe);
        let pool = pool(1, 64);
        assert!(pool.shutdown(Duration::from_secs(10)));
        let session = pool.session(opener);

        commit_without_waiting(&session, writes(Address::repeat_byte(10), 725, 0, 0));

        let stats = session.state().stats();
        assert_eq!((stats.chunks_dropped, stats.slots_dropped), (0, 0));
        assert_eq!((stats.chunks_issued, stats.slots_issued), (0, 0));
        assert!(!session.finish(1, Duration::ZERO), "a stopped pool reported a prefetched block");
        assert_eq!(probe.opened.load(Ordering::SeqCst), 0);
    }
}
