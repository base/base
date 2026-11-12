//! Bounded serial post-state computation for disposable deferred-root experiments.
//!
//! Computation does not certify a block. Only the validator wrapper may publish a validated
//! entry after every inner check succeeds. Builder and validator work never share certification.

use std::{collections::{BTreeMap, BTreeSet, VecDeque}, fmt, io, panic::{AssertUnwindSafe, catch_unwind}, sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc}, thread::{self, JoinHandle}, time::{Duration, Instant}};
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

/// One serial worker and bounded, branch-indexed ready retention.
#[derive(Debug)]
pub struct PostStateRootManager {
    /// Admission and completion limits.
    pub limits: PostStateRootLimits,
    /// Owned queue sender, closed before draining the worker.
    pub sender: Mutex<Option<mpsc::SyncSender<PostStateRootWork>>>,
    /// Worker retained for explicit accounting/drain; provider calculations are not preemptible.
    pub worker: Mutex<Option<JoinHandle<()>>>,
    /// Terminal operational failure, never invalid ancestry.
    pub failed: Arc<AtomicBool>,
    /// Ready data published only by the relevant successful execution owner.
    pub ready: Mutex<PostStateRootEntries>,
    /// Outstanding builder identities; certification waits for these results.
    pub builders: Mutex<BTreeSet<PostStateRootKey>>,
    /// Exact admitted provider operations for per-run root-work accounting.
    pub admitted: AtomicUsize,
}

impl PostStateRootManager {
    /// Constructs one bounded serial provider worker. Call lazily at the first active block.
    pub fn new(limits: PostStateRootLimits) -> ProviderResult<Self> {
        if limits.queued == 0 || limits.ready == 0 || limits.timeout.is_zero() {
            return Err(Self::error("post-state root limits must be positive"));
        }
        let (sender, receiver) = mpsc::sync_channel::<PostStateRootWork>(limits.queued);
        let failed = Arc::new(AtomicBool::new(false)); let health = failed.clone();
        let worker = thread::Builder::new().name("base-post-state-root".into()).spawn(move || {
            while let Ok(work) = receiver.recv() {
                PostStateRootMetrics::jobs_started().increment(1);
                PostStateRootMetrics::queue_lag().record(work.enqueued.elapsed().as_secs_f64());
                let start = Instant::now();
                let result = if health.load(Ordering::Acquire) || work.cancelled.load(Ordering::Acquire) {
                    Err(Self::error("post-state root work cancelled"))
                } else {
                    catch_unwind(AssertUnwindSafe(work.operation)).unwrap_or_else(|_| Err(Self::error("post-state root worker panicked")))
                };
                PostStateRootMetrics::job_duration().record(start.elapsed().as_secs_f64());
                PostStateRootMetrics::jobs_completed().increment(1);
                if result.is_err() { PostStateRootMetrics::jobs_failed().increment(1); health.store(true, Ordering::Release); }
                if !work.cancelled.load(Ordering::Acquire) && work.result.send(result).is_err() && !work.detached.load(Ordering::Acquire) {
                    health.store(true, Ordering::Release);
                }
            }
        }).map_err(Self::error)?;
        Ok(Self { limits, sender: Mutex::new(Some(sender)), worker: Mutex::new(Some(worker)), failed, ready: Mutex::default(), builders: Mutex::default(), admitted: AtomicUsize::new(0) })
    }

    /// Creates an engine-fatal provider error rather than a consensus-invalid payload error.
    pub fn error(message: impl fmt::Display) -> ProviderError { ProviderError::other(io::Error::other(message.to_string())) }

    /// Applies bounded backpressure and returns a single private completion owner.
    pub fn enqueue(&self, operation: PostStateRootOperation) -> ProviderResult<PostStateRootCompletion> {
        let deadline = Instant::now() + self.limits.timeout;
        let cancelled = Arc::new(AtomicBool::new(false)); let (result, receiver) = mpsc::sync_channel(1);
        let detached = Arc::new(AtomicBool::new(false));
        let mut work = PostStateRootWork { operation, result, cancelled: cancelled.clone(), detached: detached.clone(), enqueued: Instant::now() };
        loop {
            if self.failed.load(Ordering::Acquire) { return Err(Self::error("post-state root manager is unhealthy")); }
            let sender = self.sender.lock().map_err(Self::error)?;
            match sender.as_ref().ok_or_else(|| Self::error("post-state root manager stopped"))?.try_send(work) {
                Ok(()) => { self.admitted.fetch_add(1, Ordering::Release); break; }
                Err(mpsc::TrySendError::Full(pending)) => work = pending,
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(Self::error("post-state root worker dropped")),
            }
            drop(sender);
            if Instant::now() >= deadline {
                self.failed.store(true, Ordering::Release);
                return Err(Self::error("post-state root admission timed out"));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(PostStateRootCompletion { receiver, cancelled, failed: self.failed.clone(), deadline, detached, completed: false })
    }

    /// Queues owned builder state without awaiting its current root on the seal path.
    pub fn enqueue_builder(self: &Arc<Self>, key: PostStateRootKey, operation: PostStateRootOperation) -> ProviderResult<()> {
        {
            let mut builders = self.builders.lock().map_err(Self::error)?;
            if builders.len() >= self.limits.queued + 1 || !builders.insert(key) {
                return Err(Self::error("builder root admission capacity or duplicate identity"));
            }
        }
        let manager = self.clone();
        match self.enqueue(Box::new(move || {
            let result = operation().and_then(|data| {
                manager.publish(key, PostStateRootSource::Builder, data.clone())?; Ok(data)
            });
            manager.builders.lock().map_err(Self::error)?.remove(&key);
            result
        })) {
            Ok(completion) => { completion.detach(); Ok(()) }
            Err(error) => { self.builders.lock().map_err(Self::error)?.remove(&key); Err(error) }
        }
    }

    /// Waits for associated builder work before independent validator certification.
    /// Absence means imported data, not permission to coalesce validator computation.
    pub fn wait_builder(&self, key: PostStateRootKey) -> ProviderResult<()> {
        let deadline = Instant::now() + self.limits.timeout;
        while self.builders.lock().map_err(Self::error)?.contains(&key) {
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
        if !ready.entries.contains_key(&(key, source)) {
            if ready.entries.len() == self.limits.ready {
                let evict = ready.order.iter().position(|(old_key, old_source)| {
                    *old_source == PostStateRootSource::Validator || ready.entries.contains_key(&(*old_key, PostStateRootSource::Validator))
                });
                let Some(evict) = evict else {
                    self.failed.store(true, Ordering::Release);
                    return Err(Self::error("ready retention full with unverified builder results"));
                };
                let oldest = ready.order.remove(evict).expect("selected ready entry exists");
                ready.entries.remove(&oldest);
            }
            ready.order.push_back((key, source));
        }
        ready.entries.insert((key, source), data);
        Ok(())
    }

    /// Closes admission and accounts for all owned work within a bounded drain interval.
    /// A provider call already running cannot be preempted. A drain timeout reports failure.
    pub fn drain(&self) -> ProviderResult<()> {
        self.sender.lock().map_err(Self::error)?.take();
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
