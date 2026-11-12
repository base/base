//! Bounded serial post-state computation for disposable deferred-root experiments.
//!
//! Computation does not certify a block. Only the validator wrapper may publish a validated
//! entry after every inner check succeeds. Builder and validator work never share certification.

use std::{collections::{BTreeMap, VecDeque}, fmt, io, panic::{AssertUnwindSafe, catch_unwind}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc}, thread::{self, JoinHandle}, time::{Duration, Instant}};
use alloy_primitives::B256;
use reth_storage_api::errors::{ProviderError, ProviderResult};
use reth_trie_common::{HashedPostState, SortedTrieData, updates::TrieUpdates};

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
        let sorted = SortedTrieData::new(Arc::new(hashed.clone_into_sorted()), Arc::new(updates.clone_into_sorted()));
        Self { root, updates: Arc::new(updates), sorted }
    }
}

/// Limits on queue admission, ready retention and fallible waits.
#[derive(Debug, Clone, Copy)]
pub struct PostStateRootLimits {
    /// Maximum queued jobs, including a fixed side-branch allowance.
    pub queued: usize,
    /// Maximum ready entries retained per execution source.
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
    /// Private result, never an unbounded LazyTrieData producer.
    pub result: mpsc::SyncSender<ProviderResult<PostStateRootData>>,
    /// Cooperative cancellation checked before and after the indivisible provider calculation.
    pub cancelled: Arc<AtomicBool>,
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
    /// Shared terminal health bit.
    pub failed: Arc<AtomicBool>,
    /// End-to-end deadline including queue admission.
    pub deadline: Instant,
}

impl PostStateRootCompletion {
    /// Returns actual ready data or a terminal operational error within the configured deadline.
    pub fn wait(self) -> ProviderResult<PostStateRootData> {
        let result = self.receiver.recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| PostStateRootManager::error(format!("post-state root completion failed: {error}")))
            .and_then(|result| result);
        if result.is_err() { self.failed.store(true, Ordering::Release); }
        result
    }
}

impl Drop for PostStateRootCompletion {
    fn drop(&mut self) { self.cancelled.store(true, Ordering::Release); }
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
                let result = if health.load(Ordering::Acquire) || work.cancelled.load(Ordering::Acquire) {
                    Err(Self::error("post-state root work cancelled"))
                } else {
                    catch_unwind(AssertUnwindSafe(work.operation)).unwrap_or_else(|_| Err(Self::error("post-state root worker panicked")))
                };
                if result.is_err() { health.store(true, Ordering::Release); }
                if !work.cancelled.load(Ordering::Acquire) && work.result.send(result).is_err() {
                    health.store(true, Ordering::Release);
                }
            }
        }).map_err(Self::error)?;
        Ok(Self { limits, sender: Mutex::new(Some(sender)), worker: Mutex::new(Some(worker)), failed, ready: Mutex::default() })
    }

    /// Creates an engine-fatal provider error rather than a consensus-invalid payload error.
    pub fn error(message: impl fmt::Display) -> ProviderError { ProviderError::other(io::Error::other(message.to_string())) }

    /// Applies bounded backpressure and returns a single private completion owner.
    pub fn enqueue(&self, operation: PostStateRootOperation) -> ProviderResult<PostStateRootCompletion> {
        let deadline = Instant::now() + self.limits.timeout;
        let cancelled = Arc::new(AtomicBool::new(false)); let (result, receiver) = mpsc::sync_channel(1);
        let mut work = PostStateRootWork { operation, result, cancelled: cancelled.clone() };
        loop {
            if self.failed.load(Ordering::Acquire) { return Err(Self::error("post-state root manager is unhealthy")); }
            let sender = self.sender.lock().map_err(Self::error)?;
            match sender.as_ref().ok_or_else(|| Self::error("post-state root manager stopped"))?.try_send(work) {
                Ok(()) => break,
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
        Ok(PostStateRootCompletion { receiver, cancelled, failed: self.failed.clone(), deadline })
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
        if !ready.entries.contains_key(&(key, source)) { ready.order.push_back((key, source)); }
        ready.entries.insert((key, source), data);
        while ready.entries.len() > self.limits.ready {
            if let Some(oldest) = ready.order.pop_front() { ready.entries.remove(&oldest); }
        }
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
}
