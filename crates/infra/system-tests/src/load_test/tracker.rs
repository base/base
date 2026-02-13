use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use dashmap::DashMap;

/// Thread-safe tracker for transaction lifecycle during load tests.
#[derive(Debug)]
pub(crate) struct TransactionTracker {
    // Pending transactions (tx_hash -> send_time)
    pending: DashMap<B256, Instant>,

    // Included transactions (succeeded)
    included: DashMap<B256, ()>,

    // Reverted transactions (included but status == false)
    reverted: DashMap<B256, ()>,

    // Timed out transactions
    timed_out: DashMap<B256, ()>,

    // Send errors (not transaction-specific)
    send_errors: AtomicU64,

    // Test metadata
    test_start: Instant,
    test_completed: AtomicBool,
}

impl TransactionTracker {
    /// Creates a new tracker wrapped in an `Arc`.
    pub(crate) fn new(_test_duration: Duration) -> Arc<Self> {
        Arc::new(Self {
            pending: DashMap::new(),
            included: DashMap::new(),
            reverted: DashMap::new(),
            timed_out: DashMap::new(),
            send_errors: AtomicU64::new(0),
            test_start: Instant::now(),
            test_completed: AtomicBool::new(false),
        })
    }

    /// Records a transaction as sent with its send timestamp.
    pub(crate) fn record_sent(&self, tx_hash: B256, send_time: Instant) {
        self.pending.insert(tx_hash, send_time);
    }

    /// Increments the send error counter.
    pub(crate) fn record_send_error(&self) {
        self.send_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a transaction that was included and succeeded (status == true)
    pub(crate) fn record_included(&self, tx_hash: B256) {
        self.pending.remove(&tx_hash);
        self.included.insert(tx_hash, ());
    }

    /// Record a transaction that was included but reverted (status == false)
    pub(crate) fn record_reverted(&self, tx_hash: B256) {
        self.pending.remove(&tx_hash);
        self.reverted.insert(tx_hash, ());
    }

    /// Records a transaction as timed out.
    pub(crate) fn record_timeout(&self, tx_hash: B256) {
        if self.pending.remove(&tx_hash).is_some() {
            self.timed_out.insert(tx_hash, ());
        }
    }

    /// Returns all currently pending transactions.
    pub(crate) fn get_pending(&self) -> Vec<(B256, Instant)> {
        self.pending.iter().map(|entry| (*entry.key(), *entry.value())).collect()
    }

    /// Marks the test as completed.
    pub(crate) fn mark_test_completed(&self) {
        self.test_completed.store(true, Ordering::Relaxed);
    }

    /// Returns true if the test has been marked as completed.
    pub(crate) fn is_test_completed(&self) -> bool {
        self.test_completed.load(Ordering::Relaxed)
    }

    /// Returns true if there are no pending transactions.
    pub(crate) fn all_resolved(&self) -> bool {
        self.pending.is_empty()
    }

    /// Returns the elapsed time since the tracker was created.
    pub(crate) fn elapsed(&self) -> Duration {
        self.test_start.elapsed()
    }

    /// Returns the total number of transactions sent (all states combined).
    pub(crate) fn total_sent(&self) -> u64 {
        (self.pending.len() + self.included.len() + self.reverted.len() + self.timed_out.len())
            as u64
    }

    /// Returns the total number of included transactions.
    pub(crate) fn total_included(&self) -> u64 {
        self.included.len() as u64
    }

    /// Returns the total number of reverted transactions.
    pub(crate) fn total_reverted(&self) -> u64 {
        self.reverted.len() as u64
    }

    /// Returns the total number of pending transactions.
    pub(crate) fn total_pending(&self) -> u64 {
        self.pending.len() as u64
    }

    /// Returns the total number of timed out transactions.
    pub(crate) fn total_timed_out(&self) -> u64 {
        self.timed_out.len() as u64
    }

    /// Returns the total number of send errors.
    pub(crate) fn total_send_errors(&self) -> u64 {
        self.send_errors.load(Ordering::Relaxed)
    }
}
