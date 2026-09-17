//! Prewarm worker leases, jobs, the shared worker pool, and per-build job handle.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread,
};

use reth_execution_cache::{CachedStateProvider, ExecutionCache};
use reth_storage_api::{StateProviderBox, errors::ProviderResult};
use tracing::warn;

use super::{JobCompletion, PrewarmConfig, PrewarmScheduler, TARGET, WarmJob};
use crate::metrics::PrewarmMetrics;

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
                WarmJob::Simulate(job) => {
                    PrewarmMetrics::sim_executions_total().increment(1);
                    // Isolate the full EVM run: a simulation panic (edge-case opcode or
                    // upstream bug) must not unwind out of the worker loop and terminate
                    // this worker permanently, which would degrade throughput for the
                    // rest of the process. The overlay is throwaway, so nothing the
                    // simulation touched can be left inconsistent.
                    let simulate = &job.simulate;
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        simulate(&provider);
                    }))
                    .is_err()
                    {
                        PrewarmMetrics::sim_panics_total().increment(1);
                        warn!(target: TARGET, tx_hash = %job.tx_hash, "prewarm simulation panicked");
                    }
                    // Mark the simulation complete only once its reads have landed (or it
                    // panicked), so a build loop that reaches this transaction earlier is
                    // counted as having overtaken the warm and the pending count never
                    // permanently drifts.
                    self.scheduler.finish_simulation(&job.tx_hash);
                }
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
