//! Per-build prewarm scheduler and worker-completion signal.

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::{TxHash, map::HashSet};
use base_execution_txpool::BasePooledTx;

use super::{KeyQueue, PrewarmConfig, SimJob, SimSchedulerState, WarmJob, WarmKey};
use crate::metrics::PrewarmMetrics;

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
    /// Whether transaction-simulation warming is enabled for this build.
    pub simulate: bool,
    /// Simulation scheduling and in-flight state.
    pub sim: Mutex<SimSchedulerState>,
    /// Set when the queue refuses a simulation job.
    pub sim_saturated: AtomicBool,
}

impl std::fmt::Debug for PrewarmScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrewarmScheduler")
            .field("lookahead", &self.lookahead)
            .field("key_cap", &self.key_cap)
            .field("saturated", &self.saturated.load(Ordering::Relaxed))
            .field("simulate", &self.simulate)
            .field("sim_saturated", &self.sim_saturated.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl PrewarmScheduler {
    /// Creates a scheduler for one build.
    pub fn new(config: &PrewarmConfig) -> Self {
        // The queue buffer is bounded by the per-build work budget: the scheduler never
        // schedules more than `key_cap` keys, plus at most `sim_lookahead` simulations
        // outstanding when simulation warming is on, so the outstanding set is bounded
        // the same way.
        let capacity = if config.simulate {
            config.key_cap.saturating_add(config.sim_lookahead)
        } else {
            config.key_cap
        };
        Self {
            queue: KeyQueue::new(capacity.max(1)),
            scheduled: Mutex::new(HashSet::default()),
            lookahead: config.lookahead,
            key_cap: config.key_cap,
            saturated: AtomicBool::new(false),
            simulate: config.simulate,
            sim: Mutex::new(SimSchedulerState::default()),
            sim_saturated: AtomicBool::new(false),
        }
    }

    /// Returns the bounded lookahead in transactions.
    pub const fn lookahead(&self) -> usize {
        self.lookahead
    }

    /// Returns whether no further predicate-key work will be accepted for this build.
    pub fn is_saturated(&self) -> bool {
        self.saturated.load(Ordering::Relaxed) || self.queue.is_closed()
    }

    /// Returns whether no further simulation work will be accepted for this build.
    pub fn is_sim_saturated(&self) -> bool {
        !self.simulate || self.sim_saturated.load(Ordering::Relaxed) || self.queue.is_closed()
    }

    /// Returns whether the lookahead cursor should still be advanced.
    ///
    /// With simulation warming off this is exactly "predicate-key scheduling is still
    /// accepting work"; with it on, the cursor also keeps moving while only the
    /// simulation budget remains.
    pub fn should_advance_cursor(&self) -> bool {
        if self.queue.is_closed() {
            return false;
        }
        !self.saturated.load(Ordering::Relaxed)
            || (self.simulate && !self.sim_saturated.load(Ordering::Relaxed))
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

    /// Schedules one transaction simulation, deduplicating against simulations already
    /// scheduled for this build.
    ///
    /// Returns `true` when the job is accounted as scheduled — including duplicates, which
    /// are counted as deduplicated. Returns `false` when it was not scheduled (the queue
    /// refused it, or simulation is off). Never blocks.
    pub fn schedule_simulation(&self, job: SimJob) -> bool {
        if self.is_sim_saturated() {
            return false;
        }
        let tx_hash = job.tx_hash;
        let mut sim = self.sim.lock().expect("prewarm scheduler mutex poisoned");
        if !sim.scheduled.insert(tx_hash) {
            drop(sim);
            PrewarmMetrics::sim_jobs_deduped_total().increment(1);
            return true;
        }
        sim.pending.insert(tx_hash);
        drop(sim);
        if self.queue.push(WarmJob::Simulate(Arc::new(job))) {
            PrewarmMetrics::sim_jobs_scheduled_total().increment(1);
            true
        } else {
            // Queue closed or full: drop the job and stop scheduling simulations for this
            // build. Never block the build loop.
            let mut sim = self.sim.lock().expect("prewarm scheduler mutex poisoned");
            sim.scheduled.remove(&tx_hash);
            sim.pending.remove(&tx_hash);
            drop(sim);
            self.sim_saturated.store(true, Ordering::Relaxed);
            PrewarmMetrics::sim_jobs_dropped_total().increment(1);
            false
        }
    }

    /// Records that a worker finished simulating `tx_hash`.
    pub fn finish_simulation(&self, tx_hash: &TxHash) {
        self.sim.lock().expect("prewarm scheduler mutex poisoned").pending.remove(tx_hash);
    }

    /// Returns the number of scheduled simulations that have not finished executing.
    pub fn pending_simulations(&self) -> usize {
        self.sim.lock().expect("prewarm scheduler mutex poisoned").pending.len()
    }

    /// Returns whether `tx_hash` has a scheduled simulation that has not finished.
    pub fn simulation_pending(&self, tx_hash: &TxHash) -> bool {
        self.sim.lock().expect("prewarm scheduler mutex poisoned").pending.contains(tx_hash)
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
