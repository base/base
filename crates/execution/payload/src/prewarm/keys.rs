//! Warmable predicate-state keys and the bounded work queue.

use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
};

use alloy_primitives::{Address, StorageKey, U256};
use base_execution_txpool::ValidityPredicate;
use reth_execution_cache::CachedStateProvider;
use reth_storage_api::{AccountReader, StateProvider};
use tracing::warn;

use super::{TARGET, WarmJob};
use crate::metrics::PrewarmMetrics;

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
