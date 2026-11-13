//! Bounded serial post-state computation for disposable deferred-root experiments.
//!
//! Computation does not certify a block. Only the validator wrapper may publish a validated
//! entry after every inner check succeeds. Builder and validator work never share certification.

use std::{collections::{BTreeMap, VecDeque}, fmt, io, panic::{AssertUnwindSafe, catch_unwind}, sync::{Arc, Condvar, Mutex, OnceLock, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc}, thread::{self, JoinHandle}, time::{Duration, Instant}};
use alloy_primitives::B256;
use reth_storage_api::errors::{ProviderError, ProviderResult};
use reth_trie_common::{HashedPostState, SortedTrieData, updates::TrieUpdates};

use crate::metrics::PostStateRootMetrics;

/// Explicit benchmark controls; ordinary is unchanged and deferred requires a custom fork.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PostStateRootCohort {
    /// O: ordinary stock root algorithm and trusted insertion.
    #[default]
    Ordinary,
    /// S: serial provider root with trusted insertion.
    SerialTrusted,
    /// R: serial provider root with independent validator re-execution.
    SerialReexecution,
    /// D: delayed commitment and independent validator re-execution.
    Deferred,
}

impl std::str::FromStr for PostStateRootCohort {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value { "ordinary" => Ok(Self::Ordinary), "serial-trusted" => Ok(Self::SerialTrusted), "serial-reexecution" => Ok(Self::SerialReexecution), "deferred" => Ok(Self::Deferred), _ => Err("expected ordinary, serial-trusted, serial-reexecution or deferred") }
    }
}

/// Shared lazy root service for a builder/validator pair on one disposable custom chain.
#[derive(Debug, Default)]
pub struct PostStateRootService {
    /// Configuration-only activation timestamp. Unset means Never.
    pub activation: Option<u64>,
    /// Matched insertion/root-algorithm experiment; a scheduled fork selects D.
    pub cohort: PostStateRootCohort,
    /// Explicit admission, retention and deadline limits.
    pub limits: PostStateRootLimits,
    /// Constructed only when the first active operation is admitted.
    pub manager: OnceLock<Arc<PostStateRootManager>>,
    /// Serializes first initialization so concurrent callers never create extra workers.
    pub initializing: Mutex<()>,
}

impl PostStateRootService {
    /// Resolves the configured activation on a non-genesis block.
    pub fn active(&self, timestamp: u64, number: u64) -> bool { number != 0 && self.activation.is_some_and(|at| timestamp >= at) }
    /// Selects active serial work in S/R, or only post-fork work in D.
    pub fn controlled(&self, timestamp: u64, number: u64) -> bool { number != 0 && (matches!(self.cohort, PostStateRootCohort::SerialTrusted | PostStateRootCohort::SerialReexecution) || self.active(timestamp, number)) }
    /// Keeps ordinary/S trusted insertion, while R and active D independently re-execute.
    pub fn reexecution(&self, timestamp: u64, number: u64) -> bool { self.active(timestamp, number) || (number != 0 && self.cohort == PostStateRootCohort::SerialReexecution) }
    /// Starts a single serial worker lazily; Never and pre-fork callers must not call this.
    pub fn manager(&self) -> ProviderResult<Arc<PostStateRootManager>> {
        if let Some(manager) = self.manager.get() { return Ok(manager.clone()); }
        let _initializing = self.initializing.lock().map_err(PostStateRootManager::error)?;
        if let Some(manager) = self.manager.get() { return Ok(manager.clone()); }
        let manager = Arc::new(PostStateRootManager::new(self.limits)?);
        self.manager.set(manager.clone()).map_err(|_| PostStateRootManager::error("root manager initialized twice"))?;
        Ok(manager)
    }
}

/// Immutable sealed block identity. Height alone is never a root lookup key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PostStateRootKey {
    /// Sealed block hash.
    pub hash: B256,
    /// Sealed parent hash.
    pub parent_hash: B256,
    /// Absolute block height.
    pub number: u64,
}

/// Independent source of finalized root input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PostStateRootSource {
    /// Builder execution, not validation certification.
    Builder,
    /// Independently re-executed validator state.
    Validator,
}

/// Fully materialized actual post-state, never a delayed header commitment.
#[derive(Debug, Clone)]
pub struct PostStateRootData {
    /// Actual same-height MPT root.
    pub root: B256,
    /// Actual same-height node updates.
    pub updates: Arc<TrieUpdates>,
    /// Sorted current state and nodes, ready before engine admission.
    pub sorted: SortedTrieData,
}

impl PostStateRootData {
    /// Materializes current state and trie nodes before returning a successful completion.
    pub fn new(root: B256, hashed: &HashedPostState, updates: TrieUpdates) -> Self {
        let start = Instant::now();
        let sorted = SortedTrieData::new(Arc::new(hashed.clone_into_sorted()), Arc::new(updates.clone_into_sorted()));
        PostStateRootMetrics::sorting_duration().record(start.elapsed().as_secs_f64());
        Self { root, updates: Arc::new(updates), sorted }
    }
}

/// Limits on queue admission, ready retention and fallible waits.
#[derive(Debug, Clone, Copy)]
pub struct PostStateRootLimits {
    /// Maximum queued jobs, including a fixed side-branch allowance.
    pub queued: usize,
    /// Maximum ready entries retained across both independent execution sources.
    pub ready: usize,
    /// Maximum admission/completion wait.
    pub timeout: Duration,
}

impl Default for PostStateRootLimits {
    fn default() -> Self { Self { queued: 6, ready: 128, timeout: Duration::from_secs(30) } }
}

/// Owned root operation. The closure must own its finalized input and stable parent snapshot.
pub type PostStateRootOperation = Box<dyn FnOnce() -> ProviderResult<PostStateRootData> + Send>;

/// A queued operation and its private fallible publication channel.
pub struct PostStateRootWork {
    /// Builder identity eligible for removal only while still queued.
    pub builder: Option<PostStateRootKey>,
    /// Root operation; no borrowed execution/provider state may escape into this closure.
    pub operation: PostStateRootOperation,
    /// Monotonic admission timestamp for queue lag accounting.
    pub enqueued: Instant,
    /// Private result, never an unbounded LazyTrieData producer.
    pub result: mpsc::SyncSender<ProviderResult<PostStateRootData>>,
    /// Cooperative cancellation checked before and after the indivisible provider calculation.
    pub cancelled: Arc<AtomicBool>,
    /// Explicit detached builder ownership, distinct from abandonment.
    pub detached: Arc<AtomicBool>,
}

impl fmt::Debug for PostStateRootWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.debug_struct("PostStateRootWork").finish_non_exhaustive() }
}

/// Owned pending operations, serialized with worker pop and candidate cancellation.
#[derive(Debug, Default)]
pub struct PostStateRootQueue {
    /// Operations that have not entered the indivisible provider computation.
    pub work: VecDeque<PostStateRootWork>,
    /// Admission is closed while remaining owned work drains.
    pub closed: bool,
}

/// One bounded wait owner. Dropping it cancels unpublished work.
#[derive(Debug)]
pub struct PostStateRootCompletion {
    /// Completion receiver.
    pub receiver: mpsc::Receiver<ProviderResult<PostStateRootData>>,
    /// Cooperative cancellation token retained by the worker.
    pub cancelled: Arc<AtomicBool>,
    /// Explicit detached builder ownership, distinct from abandonment.
    pub detached: Arc<AtomicBool>,
    /// Shared terminal health bit.
    pub failed: Arc<AtomicBool>,
    /// End-to-end deadline including queue admission.
    pub deadline: Instant,
    /// Whether this owner consumed its result.
    pub completed: bool,
}

impl PostStateRootCompletion {
    /// Returns actual ready data or a terminal operational error within the configured deadline.
    pub fn wait(mut self) -> ProviderResult<PostStateRootData> {
        let start = Instant::now();
        let mut result = self.receiver.recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| PostStateRootManager::error(format!("post-state root completion failed: {error}")))
            .and_then(|result| result);
        PostStateRootMetrics::completion_wait().record(start.elapsed().as_secs_f64());
        if self.failed.load(Ordering::Acquire) { result = Err(PostStateRootManager::error("post-state root manager is unhealthy")); }
        if result.is_err() { self.failed.store(true, Ordering::Release); }
        self.completed = true;
        result
    }
}

impl PostStateRootCompletion {
    /// Transfers completion ownership to the bounded builder-pending registry.
    pub fn detach(self) { self.detached.store(true, Ordering::Release); }
}

impl Drop for PostStateRootCompletion {
    fn drop(&mut self) {
        if !self.detached.load(Ordering::Acquire) {
            self.cancelled.store(true, Ordering::Release);
            if !self.completed { self.failed.store(true, Ordering::Release); }
        }
    }
}

/// Bounded ready entries with insertion order independent of height and branch.
#[derive(Debug, Default)]
pub struct PostStateRootEntries {
    /// Exact identity and independent execution source.
    pub entries: BTreeMap<(PostStateRootKey, PostStateRootSource), PostStateRootData>,
    /// FIFO ready retention order.
    pub order: VecDeque<(PostStateRootKey, PostStateRootSource)>,
}

/// Finalized input and export ownership for one native sealed identity.
#[derive(Debug)]
pub struct PostStateRootCandidate {
    /// Exact authoritative state; duplicate identity may not hide different execution.
    pub hashed: HashedPostState,
    /// Shared payload ownership, excluding the worker and manager.
    pub lease: std::sync::Weak<PostStateRootCandidateLease>,
    /// Exported payloads remain protected even if a later FCU abandons them.
    pub selected: bool,
}

/// Clone-shared ownership of a never-exported native candidate.
/// Engine conversions and executed/sealed extraction select it; block inspection does not.
/// Arbitrary raw block clones are outside builder comparison ownership coverage. An exported
/// candidate remains protected even after a later FCU abandons it; unmatched exports reaching
/// the configured retention bound stop the experiment rather than losing comparisons.
#[derive(Debug)]
pub struct PostStateRootCandidateLease {
    /// Owning service worker, retained independently by the runtime.
    pub manager: std::sync::Weak<PostStateRootManager>,
    /// Exact sealed identity.
    pub key: PostStateRootKey,
}

impl PostStateRootCandidateLease {
    /// Protects an exported candidate until real independent validation or S insertion.
    pub fn select(&self) {
        if let Some(manager) = self.manager.upgrade() {
            match manager.candidates.lock() {
                Ok(mut candidates) => { if let Some(candidate) = candidates.get_mut(&self.key) { candidate.selected = true; } }
                Err(_) => { manager.failed.store(true, Ordering::Release); }
            }
        }
    }
}

impl Drop for PostStateRootCandidateLease {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.upgrade() {
            let retire = || -> ProviderResult<()> {
                let mut candidates = manager.candidates.lock().map_err(PostStateRootManager::error)?;
                if let Some(candidate) = candidates.get(&self.key) {
                    // A fresh owner may have reacquired this identity while this destructor waited.
                    if candidate.lease.as_ptr() != self as *const Self { return Ok(()); }
                    if candidate.selected {
                        if manager.get(self.key, PostStateRootSource::Validator)?.is_some() && !manager.builders.lock().map_err(PostStateRootManager::error)?.contains_key(&self.key) { candidates.remove(&self.key); }
                        return Ok(());
                    }
                    manager.retire_trusted_builder(self.key)?;
                    let mut queue = manager.queue.0.lock().map_err(PostStateRootManager::error)?;
                    if let Some(index) = queue.work.iter().position(|work| work.builder == Some(self.key)) {
                        queue.work.remove(index);
                        manager.builders.lock().map_err(PostStateRootManager::error)?.remove(&self.key);
                        manager.queue.1.notify_all();
                    }
                    if !manager.builders.lock().map_err(PostStateRootManager::error)?.contains_key(&self.key) { candidates.remove(&self.key); }
                }
                Ok(())
            };
            if retire().is_err() { manager.failed.store(true, Ordering::Release); }
        }
    }
}

/// One serial worker and bounded, branch-indexed ready retention.
#[derive(Debug)]
pub struct PostStateRootManager {
    /// Admission and completion limits.
    pub limits: PostStateRootLimits,
    /// Bounded owned queue and wakeups for admission, pop, cancellation and shutdown.
    pub queue: Arc<(Mutex<PostStateRootQueue>, Condvar)>,
    /// Worker retained for explicit accounting/drain; provider calculations are not preemptible.
    pub worker: Mutex<Option<JoinHandle<()>>>,
    /// Terminal operational failure, never invalid ancestry.
    pub failed: Arc<AtomicBool>,
    /// Ready data published only by the relevant successful execution owner.
    pub ready: Mutex<PostStateRootEntries>,
    /// Outstanding builder identities; certification waits for these results.
    pub builders: Mutex<BTreeMap<PostStateRootKey, Instant>>,
    /// Exact admitted operations, including unused work canceled before provider computation.
    pub admitted: AtomicUsize,
    /// Actual operations invoked by the worker, excluding canceled queued candidates.
    pub computed: Arc<AtomicUsize>,
    /// Native candidate ownership; exported unmatched comparisons are never silently evicted.
    pub candidates: Mutex<BTreeMap<PostStateRootKey, PostStateRootCandidate>>,
}

impl PostStateRootManager {
    /// Constructs one bounded serial provider worker. Call lazily at the first active block.
    pub fn new(limits: PostStateRootLimits) -> ProviderResult<Self> {
        if limits.queued == 0 || limits.ready == 0 || limits.timeout.is_zero() {
            return Err(Self::error("post-state root limits must be positive"));
        }
        let queue = Arc::new((Mutex::new(PostStateRootQueue::default()), Condvar::new()));
        let pending = queue.clone();
        let computed = Arc::new(AtomicUsize::new(0)); let computations = computed.clone();
        let failed = Arc::new(AtomicBool::new(false)); let health = failed.clone();
        let worker = thread::Builder::new().name("base-post-state-root".into()).spawn(move || {
            loop {
                let work = {
                    let mut queue = pending.0.lock().expect("root queue poisoned");
                    while queue.work.is_empty() && !queue.closed { queue = pending.1.wait(queue).expect("root queue poisoned"); }
                    let Some(work) = queue.work.pop_front() else { break };
                    pending.1.notify_all();
                    work
                };
                PostStateRootMetrics::queue_lag().record(work.enqueued.elapsed().as_secs_f64());
                let start = Instant::now();
                let result = if health.load(Ordering::Acquire) || work.cancelled.load(Ordering::Acquire) {
                    Err(Self::error("post-state root work cancelled"))
                } else {
                    PostStateRootMetrics::jobs_started().increment(1);
                    computations.fetch_add(1, Ordering::Release);
                    let result = catch_unwind(AssertUnwindSafe(work.operation)).unwrap_or_else(|_| Err(Self::error("post-state root worker panicked")));
                    PostStateRootMetrics::job_duration().record(start.elapsed().as_secs_f64());
                    PostStateRootMetrics::jobs_completed().increment(1);
                    result
                };
                if result.is_err() { PostStateRootMetrics::jobs_failed().increment(1); health.store(true, Ordering::Release); }
                if !work.cancelled.load(Ordering::Acquire) && work.result.send(result).is_err() && !work.detached.load(Ordering::Acquire) {
                    health.store(true, Ordering::Release);
                }
                pending.1.notify_all();
            }
        }).map_err(Self::error)?;
        Ok(Self { limits, queue, worker: Mutex::new(Some(worker)), failed, ready: Mutex::default(), builders: Mutex::default(), admitted: AtomicUsize::new(0), computed, candidates: Mutex::default() })
    }

    /// Creates an engine-fatal provider error rather than a consensus-invalid payload error.
    pub fn error(message: impl fmt::Display) -> ProviderError { ProviderError::other(io::Error::other(message.to_string())) }

    /// Applies bounded backpressure and returns a single private completion owner.
    pub fn enqueue(&self, operation: PostStateRootOperation) -> ProviderResult<PostStateRootCompletion> {
        Ok(self.enqueue_work(operation, None)?.expect("unkeyed work cannot be a duplicate"))
    }

    /// Atomically reserves queue and pending identity capacity; waits hold only the queue lock.
    pub fn enqueue_work(&self, operation: PostStateRootOperation, builder: Option<PostStateRootKey>) -> ProviderResult<Option<PostStateRootCompletion>> {
        let deadline = Instant::now() + self.limits.timeout;
        let cancelled = Arc::new(AtomicBool::new(false)); let (result, receiver) = mpsc::sync_channel(1);
        let detached = Arc::new(AtomicBool::new(builder.is_some()));
        let work = PostStateRootWork { operation, builder, result, cancelled: cancelled.clone(), detached: detached.clone(), enqueued: Instant::now() };
        loop {
            self.check_health()?;
            let candidates = if builder.is_some() { Some(self.candidates.lock().map_err(Self::error)?) } else { None };
            let mut queue = self.queue.0.lock().map_err(Self::error)?;
            let mut builders = self.builders.lock().map_err(Self::error)?;
            if queue.closed { return Err(Self::error("post-state root manager stopped")); }
            if let Some(key) = builder {
                if builders.contains_key(&key) || self.get(key, PostStateRootSource::Builder)?.is_some() { return Ok(None); }
            }
            if Instant::now() >= deadline {
                self.failed.store(true, Ordering::Release);
                return Err(Self::error("post-state root admission timed out"));
            }
            if queue.work.len() < self.limits.queued && (builder.is_none() || builders.len() < self.limits.queued + 1) {
                if let Some(key) = builder { builders.insert(key, deadline); }
                queue.work.push_back(work);
                self.admitted.fetch_add(1, Ordering::Release);
                self.queue.1.notify_all();
                break;
            }
            drop(builders); drop(candidates);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.failed.store(true, Ordering::Release);
                return Err(Self::error("post-state root admission timed out"));
            }
            // Drop every other lock before waiting so last-owner cancellation can free a slot.
            drop(self.queue.1.wait_timeout(queue, remaining).map_err(Self::error)?);
        }
        Ok(Some(PostStateRootCompletion { receiver, cancelled, failed: self.failed.clone(), deadline, detached, completed: false }))
    }

    /// Acquires shared native payload ownership and verifies duplicate finalized state exactly.
    pub fn candidate(self: &Arc<Self>, key: PostStateRootKey, hashed: &HashedPostState) -> ProviderResult<Arc<PostStateRootCandidateLease>> {
        self.check_health()?;
        if self.get(key, PostStateRootSource::Builder)?.is_some_and(|data| *data.sorted.hashed_state != hashed.clone_into_sorted()) {
            self.failed.store(true, Ordering::Release);
            return Err(Self::error("duplicate builder identity has different finalized state"));
        }
        let mut candidates = self.candidates.lock().map_err(Self::error)?;
        if let Some(candidate) = candidates.get_mut(&key) {
            if candidate.hashed != *hashed {
                self.failed.store(true, Ordering::Release);
                return Err(Self::error("duplicate builder identity has different finalized state"));
            }
            if let Some(lease) = candidate.lease.upgrade() { return Ok(lease); }
            let lease = Arc::new(PostStateRootCandidateLease { manager: Arc::downgrade(self), key });
            candidate.lease = Arc::downgrade(&lease);
            return Ok(lease);
        }
        let lease = Arc::new(PostStateRootCandidateLease { manager: Arc::downgrade(self), key });
        candidates.insert(key, PostStateRootCandidate { hashed: hashed.clone(), lease: Arc::downgrade(&lease), selected: false });
        Ok(lease)
    }

    /// Queues owned builder state without awaiting its current root on the seal path.
    pub fn enqueue_builder(self: &Arc<Self>, key: PostStateRootKey, operation: PostStateRootOperation) -> ProviderResult<()> {
        self.check_health()?;
        let manager = self.clone();
        match self.enqueue_work(Box::new(move || {
            let result = operation().and_then(|data| {
                manager.check_health()?;
                // Serialize publication with last-owner retirement; the worker is not a payload owner.
                let candidates = manager.candidates.lock().map_err(Self::error)?;
                if candidates.get(&key).is_none_or(|candidate| candidate.selected || candidate.lease.strong_count() != 0) {
                    manager.publish(key, PostStateRootSource::Builder, data.clone())?;
                }
                Ok(data)
            });
            let mut candidates = manager.candidates.lock().map_err(Self::error)?;
            let _queue = manager.queue.0.lock().map_err(Self::error)?;
            manager.builders.lock().map_err(Self::error)?.remove(&key);
            manager.queue.1.notify_all();
            let compared = manager.ready.lock().map_err(Self::error)?.entries.contains_key(&(key, PostStateRootSource::Validator));
            if candidates.get(&key).is_some_and(|candidate| candidate.lease.strong_count() == 0 && (!candidate.selected || compared)) { candidates.remove(&key); }
            result
        }), Some(key)) {
            Ok(Some(completion)) => { completion.detach(); Ok(()) }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Checks detached deadlines even when no validator ever requests the sealed payload.
    /// The runtime health owner polls this independently of the indivisible provider worker.
    pub fn check_health(&self) -> ProviderResult<()> {
        if self.builders.lock().map_err(Self::error)?.values().any(|deadline| Instant::now() >= *deadline) {
            self.failed.store(true, Ordering::Release);
            return Err(Self::error("detached builder root completion timed out"));
        }
        if self.failed.load(Ordering::Acquire) { return Err(Self::error("post-state root manager is unhealthy")); }
        Ok(())
    }

    /// Waits for associated builder work before independent validator certification.
    /// Absence means imported data, not permission to coalesce validator computation.
    pub fn wait_builder(&self, key: PostStateRootKey) -> ProviderResult<()> {
        let deadline = Instant::now() + self.limits.timeout;
        while self.builders.lock().map_err(Self::error)?.contains_key(&key) {
            self.check_health()?;
            if self.failed.load(Ordering::Acquire) { return Err(Self::error("builder root failed")); }
            if Instant::now() >= deadline {
                self.failed.store(true, Ordering::Release);
                return Err(Self::error("builder root completion timed out"));
            }
            thread::sleep(Duration::from_millis(1));
        }
        if self.failed.load(Ordering::Acquire) { return Err(Self::error("post-state root manager is unhealthy")); }
        Ok(())
    }

    /// Retrieves a previously certified branch root without interpreting a header as post-state.
    pub fn get(&self, key: PostStateRootKey, source: PostStateRootSource) -> ProviderResult<Option<PostStateRootData>> {
        if self.failed.load(Ordering::Acquire) { return Err(Self::error("post-state root manager is unhealthy")); }
        Ok(self.ready.lock().map_err(Self::error)?.entries.get(&(key, source)).cloned())
    }

    /// Publishes after successful execution checks; independently computed sources must agree.
    pub fn publish(&self, key: PostStateRootKey, source: PostStateRootSource, data: PostStateRootData) -> ProviderResult<()> {
        if self.failed.load(Ordering::Acquire) { return Err(Self::error("post-state root manager is unhealthy")); }
        let mut ready = self.ready.lock().map_err(Self::error)?;
        for provenance in [PostStateRootSource::Builder, PostStateRootSource::Validator] {
            if let Some(previous) = ready.entries.get(&(key, provenance)) {
                if previous.root != data.root || previous.sorted != data.sorted {
                    self.failed.store(true, Ordering::Release);
                    return Err(Self::error("independent builder/validator post-state mismatch"));
                }
            }
        }
        // This builder has already compared its real root with independent accepted execution.
        if source == PostStateRootSource::Builder && ready.entries.contains_key(&(key, PostStateRootSource::Validator)) { return Ok(()); }
        if !ready.entries.contains_key(&(key, source)) {
            if ready.entries.len() == self.limits.ready {
                let evict = ready.order.iter().position(|(old_key, old_source)| {
                    *old_source == PostStateRootSource::Validator || ready.entries.contains_key(&(*old_key, PostStateRootSource::Validator))
                        || (source == PostStateRootSource::Validator && *old_key == key)
                });
                let Some(evict) = evict else {
                    self.failed.store(true, Ordering::Release);
                    return Err(Self::error("ready retention full with unverified builder results"));
                };
                let oldest = ready.order.remove(evict).expect("selected ready entry exists");
                ready.entries.remove(&oldest);
                // A matched Builder must not become protected again when its Validator ages out.
                if oldest.1 == PostStateRootSource::Validator {
                    ready.entries.remove(&(oldest.0, PostStateRootSource::Builder));
                    ready.order.retain(|entry| *entry != (oldest.0, PostStateRootSource::Builder));
                }
            }
            ready.order.push_back((key, source));
        }
        ready.entries.insert((key, source), data);
        Ok(())
    }

    /// Retires S builder provenance after successful trusted insertion, never certifying it.
    /// The last never-exported native owner also uses this removal; Validator is never removed.
    pub fn retire_trusted_builder(&self, key: PostStateRootKey) -> ProviderResult<()> {
        let mut ready = self.ready.lock().map_err(Self::error)?;
        ready.entries.remove(&(key, PostStateRootSource::Builder));
        ready.order.retain(|entry| *entry != (key, PostStateRootSource::Builder));
        Ok(())
    }

    /// Releases ownership metadata only after real validator comparison or successful S insertion.
    pub fn complete_candidate(&self, key: PostStateRootKey) -> ProviderResult<()> {
        self.candidates.lock().map_err(Self::error)?.remove(&key);
        Ok(())
    }

    /// Closes admission and accounts for all owned work within a bounded drain interval.
    /// A provider call already running cannot be preempted. A drain timeout reports failure.
    pub fn drain(&self) -> ProviderResult<()> {
        self.queue.0.lock().map_err(Self::error)?.closed = true;
        self.queue.1.notify_all();
        let deadline = Instant::now() + self.limits.timeout;
        let mut worker = self.worker.lock().map_err(Self::error)?;
        if let Some(handle) = worker.as_ref() {
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    self.failed.store(true, Ordering::Release);
                    return Err(Self::error("post-state root worker drain timed out"));
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
        if let Some(handle) = worker.take() { handle.join().map_err(|_| Self::error("post-state root worker dropped"))?; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates distinct deterministic materialized data for manager lifecycle tests, not MPT tests.
    fn data(byte: u8) -> PostStateRootData { PostStateRootData::new(B256::repeat_byte(byte), &HashedPostState::default(), TrieUpdates::default()) }
    /// Creates full branch identity, including a same-height competing fork.
    fn key(byte: u8) -> PostStateRootKey { PostStateRootKey { hash: B256::repeat_byte(byte), parent_hash: B256::ZERO, number: 1 } }

    /// Unvalidated private work cannot appear in ready lookups; sources compute independently.
    #[test]
    fn private_completion_and_independent_sources() {
        let manager = PostStateRootManager::new(PostStateRootLimits::default()).unwrap();
        let result = manager.enqueue(Box::new(|| Ok(data(1)))).unwrap().wait().unwrap();
        assert!(manager.get(key(1), PostStateRootSource::Validator).unwrap().is_none(), "completion alone must expose zero certified roots");
        manager.publish(key(1), PostStateRootSource::Builder, result).unwrap();
        assert!(manager.get(key(1), PostStateRootSource::Validator).unwrap().is_none(), "builder root must not certify validator execution");
        manager.publish(key(1), PostStateRootSource::Validator, data(1)).unwrap();
        assert_eq!(manager.get(key(1), PostStateRootSource::Validator).unwrap().unwrap().root, B256::repeat_byte(1), "independent validation publishes exact root");
        manager.drain().unwrap();
    }

    /// Ready retention is bounded, keyed by sealed hash rather than the shared height.
    #[test]
    fn bounded_ready_retention() {
        let manager = PostStateRootManager::new(PostStateRootLimits { ready: 2, ..Default::default() }).unwrap();
        for byte in 1..=3 { manager.publish(key(byte), PostStateRootSource::Validator, data(byte)).unwrap(); }
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 2, "exactly two ready entries retained");
        assert!(manager.get(key(1), PostStateRootSource::Validator).unwrap().is_none(), "oldest ready fork evicted");
        assert_eq!(manager.get(key(2), PostStateRootSource::Validator).unwrap().unwrap().root, B256::repeat_byte(2), "same-height forks keep independent roots");
        manager.drain().unwrap();
    }

    /// Pending R/D builder comparisons must never be evicted like accepted S insertions.
    #[test]
    fn unmatched_builder_provenance_remains_protected() {
        let manager = PostStateRootManager::new(PostStateRootLimits { ready: 2, ..Default::default() }).unwrap();
        manager.publish(key(1), PostStateRootSource::Builder, data(1)).unwrap();
        manager.publish(key(2), PostStateRootSource::Builder, data(2)).unwrap();
        assert_eq!(manager.publish(key(3), PostStateRootSource::Builder, data(3)).unwrap_err().to_string(), "ready retention full with unverified builder results", "unmatched R/D roots must fail capacity admission rather than evict independent comparison inputs");
        let ready = manager.ready.lock().unwrap();
        assert_eq!(ready.entries.len(), 2, "both original unmatched builder entries must remain retained");
        assert!(ready.entries.contains_key(&(key(1), PostStateRootSource::Builder)), "oldest unmatched builder root must not be evicted");
        assert!(ready.entries.contains_key(&(key(2), PostStateRootSource::Builder)), "second unmatched builder root must not be evicted");
        assert_eq!(ready.order.len(), 2, "FIFO must contain exactly the protected original entries");
        drop(ready); manager.drain().unwrap();
    }

    /// A producer panic wakes the waiter and terminally stops admission without INVALID ancestry.
    #[test]
    fn panic_is_operational_and_bounded() {
        let manager = PostStateRootManager::new(PostStateRootLimits::default()).unwrap();
        let result = manager.enqueue(Box::new(|| panic!("injected root panic"))).unwrap().wait();
        assert!(result.is_err(), "worker panic must produce a fallible operational result");
        assert!(manager.enqueue(Box::new(|| Ok(data(1)))).is_err(), "unhealthy manager rejects subsequent admission");
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 0, "panic publishes zero roots");
        manager.drain().unwrap();
    }
    /// Abandoning the sole waiter is terminal and cannot leave a successful pending root.
    #[test]
    fn abandonment_cancels_and_stops_admission() {
        let manager = PostStateRootManager::new(PostStateRootLimits::default()).unwrap();
        let completion = manager.enqueue(Box::new(|| Ok(data(1)))).unwrap();
        drop(completion);
        assert!(manager.failed.load(Ordering::Acquire), "dropped completion must immediately mark unhealthy");
        assert!(manager.enqueue(Box::new(|| Ok(data(2)))).is_err(), "abandoned work must not permit new admission");
        manager.drain().unwrap();
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 0, "abandonment must publish exactly zero roots");
    }

    /// A blocked provider bounds completion but remains owned until released and drained.
    #[test]
    fn completion_timeout_drains_indivisible_provider_work() {
        let manager = PostStateRootManager::new(PostStateRootLimits { timeout: Duration::from_millis(50), ..Default::default() }).unwrap();
        let (release, blocked) = mpsc::channel(); let (started, running) = mpsc::channel();
        let completion = manager.enqueue(Box::new(move || { started.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(1)) })).unwrap();
        running.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(completion.wait().is_err(), "stalled provider must return an operational timeout");
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 0, "timed out root must never become ready");
        release.send(()).unwrap(); manager.drain().unwrap();
    }

    /// One running and one queued operation exhaust exactly the configured queue allowance.
    #[test]
    fn admission_timeout_is_bounded() {
        let manager = PostStateRootManager::new(PostStateRootLimits { queued: 1, timeout: Duration::from_millis(50), ..Default::default() }).unwrap();
        let (release, blocked) = mpsc::channel(); let (started, running) = mpsc::channel();
        let first = manager.enqueue(Box::new(move || { started.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(1)) })).unwrap();
        running.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = manager.enqueue(Box::new(|| Ok(data(2)))).unwrap();
        assert!(manager.enqueue(Box::new(|| Ok(data(3)))).is_err(), "third operation must hit admission timeout with one running and one queued");
        release.send(()).unwrap();
        assert!(first.wait().is_err(), "terminal admission failure must prevent success from an earlier operation");
        assert!(second.wait().is_err(), "queued operation must also observe terminal health");
        manager.drain().unwrap();
    }

    /// A late native rebuild compares against accepted execution without orphaned protection.
    #[test]
    fn late_builder_comparison_does_not_outlive_validator_retention() {
        let manager = PostStateRootManager::new(PostStateRootLimits { ready: 2, ..Default::default() }).unwrap();
        manager.publish(key(1), PostStateRootSource::Validator, data(1)).unwrap();
        manager.publish(key(1), PostStateRootSource::Builder, data(1)).unwrap();
        assert!(manager.get(key(1), PostStateRootSource::Builder).unwrap().is_none(), "already compared late builder needs no unmatched slot");
        manager.publish(key(2), PostStateRootSource::Validator, data(2)).unwrap();
        manager.publish(key(3), PostStateRootSource::Validator, data(3)).unwrap();
        assert!(manager.get(key(1), PostStateRootSource::Validator).unwrap().is_none(), "old compared identity may age out after real validation");
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 2, "only two accepted current identities remain after eviction");
        manager.drain().unwrap();
    }

    /// Independent validation may consume its compared slot at capacity without losing a fork.
    #[test]
    fn comparison_at_capacity_preserves_unmatched_fork() {
        let manager = PostStateRootManager::new(PostStateRootLimits { ready: 2, ..Default::default() }).unwrap();
        manager.publish(key(1), PostStateRootSource::Builder, data(1)).unwrap();
        manager.publish(key(2), PostStateRootSource::Builder, data(2)).unwrap();
        manager.publish(key(1), PostStateRootSource::Validator, data(1)).unwrap();
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 2, "real comparison must reuse its own capacity slot");
        assert!(manager.get(key(1), PostStateRootSource::Validator).unwrap().is_some(), "independent validation must certify its exact compared identity");
        assert!(manager.get(key(2), PostStateRootSource::Builder).unwrap().is_some(), "unmatched same-height fork must remain protected");
        manager.drain().unwrap();
    }

    /// Last internal owner may retire late work, but the provider remains owned and drainable.
    #[test]
    fn candidate_drop_reacquire_and_late_completion() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits::default()).unwrap());
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let barrier = manager.enqueue(Box::new(move || { entered.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(9)) })).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let candidate = manager.candidate(key(1), &HashedPostState::default()).unwrap();
        manager.enqueue_builder(key(1), Box::new(|| panic!("canceled queued generation must never compute"))).unwrap();
        drop(candidate);
        assert_eq!(manager.builders.lock().unwrap().len(), 0, "queued unused generation must release pending identity immediately");
        let reacquired = manager.candidate(key(1), &HashedPostState::default()).unwrap();
        manager.enqueue_builder(key(1), Box::new(|| Ok(data(1)))).unwrap();
        assert_eq!(manager.admitted.load(Ordering::Acquire), 3, "canceled queued identity must admit a fresh generation after reacquisition");
        drop(reacquired);
        assert_eq!(manager.builders.lock().unwrap().len(), 0, "both canceled queued generations must release pending identity before computation");
        release.send(()).unwrap(); barrier.wait().unwrap(); manager.wait_builder(key(1)).unwrap();
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 0, "late abandoned work must publish neither Builder nor Validator");
        manager.drain().unwrap();
        assert_eq!(manager.candidates.lock().unwrap().len(), 0, "drained unselected candidate metadata must retire");
        assert_eq!(manager.computed.load(Ordering::Acquire), 1, "only the running barrier computes, neither unused queued generation does");
    }

    /// Once popped, an abandoned identity stays owned and reacquisition shares its operation.
    #[test]
    fn running_candidate_reacquisition_is_idempotent() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits::default()).unwrap());
        let candidate = manager.candidate(key(1), &HashedPostState::default()).unwrap();
        let (entered, started) = mpsc::channel(); let (release, blocked) = mpsc::channel();
        manager.enqueue_builder(key(1), Box::new(move || { entered.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(1)) })).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(candidate);
        assert_eq!(manager.builders.lock().unwrap().len(), 1, "running work must remain owned with its original deadline");
        let reacquired = manager.candidate(key(1), &HashedPostState::default()).unwrap();
        manager.enqueue_builder(key(1), Box::new(|| panic!("running identity must not compute twice"))).unwrap();
        reacquired.select();
        release.send(()).unwrap(); manager.wait_builder(key(1)).unwrap();
        assert_eq!(manager.admitted.load(Ordering::Acquire), 1, "running reacquisition admits zero additional operations");
        assert_eq!(manager.get(key(1), PostStateRootSource::Builder).unwrap().unwrap().root, data(1).root, "reacquired running result must publish for selected identity");
        drop(reacquired); manager.drain().unwrap();
    }

    /// Full admission releases ownership locks so concurrent unused cancellation can wake it.
    #[test]
    fn cancellation_wakes_bounded_builder_admission() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits { queued: 1, timeout: Duration::from_secs(2), ..Default::default() }).unwrap());
        let (entered, started) = mpsc::channel(); let (release, blocked) = mpsc::channel();
        let barrier = manager.enqueue(Box::new(move || { entered.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(9)) })).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let candidate = manager.candidate(key(1), &HashedPostState::default()).unwrap();
        manager.enqueue_builder(key(1), Box::new(|| panic!("queued orphan must not execute"))).unwrap();
        let next = manager.candidate(key(2), &HashedPostState::default()).unwrap();
        let (done, admitted) = mpsc::channel();
        let waiting = manager.clone();
        let admission = thread::spawn(move || { done.send(waiting.enqueue_builder(key(2), Box::new(|| Ok(data(2))))).unwrap(); });
        assert!(matches!(admitted.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)), "ordinary full capacity must backpressure rather than fail immediately");
        drop(candidate);
        admitted.recv_timeout(Duration::from_secs(1)).expect("cancellation must wake admission without lock deadlock").unwrap();
        admission.join().unwrap();
        assert_eq!(manager.queue.0.lock().unwrap().work.len(), 1, "queue must contain exactly the new admitted candidate within its bound");
        assert_eq!(manager.builders.lock().unwrap().len(), 1, "pending registry must contain only the new generation");
        next.select(); release.send(()).unwrap(); barrier.wait().unwrap(); manager.wait_builder(key(2)).unwrap();
        assert_eq!(manager.computed.load(Ordering::Acquire), 2, "only barrier and selected replacement compute");
        drop(next); manager.check_health().unwrap(); manager.drain().unwrap();
    }

    /// Even fully protected pending builders backpressure until worker progress, not immediately fail.
    #[test]
    fn protected_builder_capacity_waits_for_worker_progress() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits { queued: 1, timeout: Duration::from_secs(2), ..Default::default() }).unwrap());
        let first = manager.candidate(key(1), &HashedPostState::default()).unwrap(); first.select();
        let second = manager.candidate(key(2), &HashedPostState::default()).unwrap(); second.select();
        let third = manager.candidate(key(3), &HashedPostState::default()).unwrap(); third.select();
        let (entered, started) = mpsc::channel(); let (release, blocked) = mpsc::channel();
        manager.enqueue_builder(key(1), Box::new(move || { entered.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(1)) })).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        manager.enqueue_builder(key(2), Box::new(|| Ok(data(2)))).unwrap();
        let waiting = manager.clone(); let (done, response) = mpsc::channel();
        let admission = thread::spawn(move || { done.send(waiting.enqueue_builder(key(3), Box::new(|| Ok(data(3))))).unwrap(); });
        assert!(matches!(response.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)), "queued+1 protected identities must backpressure third builder without terminal failure");
        manager.check_health().unwrap();
        assert_eq!(manager.builders.lock().unwrap().len(), 2, "one running and one queued selected builder exactly exhaust pending capacity");
        assert_eq!(manager.queue.0.lock().unwrap().work.len(), 1, "pending admission cannot exceed the configured queue bound");
        release.send(()).unwrap();
        response.recv_timeout(Duration::from_secs(1)).expect("worker progress must wake protected builder admission").unwrap();
        admission.join().unwrap();
        for byte in 1..=3 {
            manager.wait_builder(key(byte)).unwrap();
            assert_eq!(manager.get(key(byte), PostStateRootSource::Builder).unwrap().unwrap().root, data(byte).root, "each selected builder must compute its own exact result");
        }
        drop((first, second, third)); manager.drain().unwrap();
        assert_eq!(manager.computed.load(Ordering::Acquire), 3, "no selected computations may be skipped to make room");
        manager.check_health().unwrap();
    }

    /// Closing admission wakes blocked callers, while drain still owns the running provider.
    #[test]
    fn drain_wakes_blocked_admission() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits { queued: 1, timeout: Duration::from_secs(2), ..Default::default() }).unwrap());
        let (entered, started) = mpsc::channel(); let (release, blocked) = mpsc::channel();
        let first = manager.enqueue(Box::new(move || { entered.send(()).unwrap(); blocked.recv().unwrap(); Ok(data(1)) })).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = manager.enqueue(Box::new(|| Ok(data(2)))).unwrap();
        let waiting = manager.clone(); let (done, response) = mpsc::channel();
        let admission = thread::spawn(move || { done.send(waiting.enqueue(Box::new(|| Ok(data(3)))).map(|_| ())).unwrap(); });
        assert!(matches!(response.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout)), "full queue must wait until capacity or shutdown");
        let draining = manager.clone(); let drain = thread::spawn(move || draining.drain());
        assert!(response.recv_timeout(Duration::from_secs(1)).expect("shutdown must wake admission").is_err(), "closed queue must reject blocked admission");
        release.send(()).unwrap(); first.wait().unwrap(); second.wait().unwrap();
        admission.join().unwrap(); drain.join().unwrap().unwrap();
        assert_eq!(manager.computed.load(Ordering::Acquire), 2, "shutdown must drain both already owned operations only");
        manager.check_health().unwrap();
    }

    /// A sealed identity may not coalesce different authoritative execution state.
    #[test]
    fn duplicate_candidate_rejects_changed_finalized_input() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits::default()).unwrap());
        let original = manager.candidate(key(1), &HashedPostState::default()).unwrap();
        let mut changed = HashedPostState::default();
        changed.accounts.insert(B256::repeat_byte(1), Some(Default::default()));
        assert_eq!(manager.candidate(key(1), &changed).unwrap_err().to_string(), "duplicate builder identity has different finalized state", "same sealed identity must compare finalized input before coalescing any work");
        assert!(manager.check_health().is_err(), "different finalized state for one identity must fail closed");
        drop(original); manager.drain().unwrap();
    }

    /// Detached builder work remains associated until the validator compares an independent root.
    #[test]
    fn builder_completion_is_not_validator_certification() {
        let manager = Arc::new(PostStateRootManager::new(PostStateRootLimits::default()).unwrap());
        manager.enqueue_builder(key(1), Box::new(|| Ok(data(1)))).unwrap();
        manager.wait_builder(key(1)).unwrap();
        assert!(manager.get(key(1), PostStateRootSource::Validator).unwrap().is_none(), "builder completion must not certify validator state");
        assert!(manager.publish(key(1), PostStateRootSource::Validator, data(2)).is_err(), "independently re-executed root mismatch must be fatal");
        assert_eq!(manager.ready.lock().unwrap().entries.len(), 1, "mismatch must publish no validator result");
        manager.drain().unwrap();
    }

}
