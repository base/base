//! Worker pool that resolves state prefetch hints against the live state provider.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use alloy_primitives::B256;
use base_precompile_storage::{PrefetchRequest, StatePrefetcher};
use reth_provider::{StateProvider, StateProviderFactory};
use tracing::trace;

use crate::PrefetchMetrics;

/// Global cap for queued requests across all workers. This stays fixed when operators increase
/// concurrency, preventing worker-count configuration from multiplying retained queue memory.
const MAX_QUEUED_REQUESTS: usize = 4_096;

/// Maximum reads served by one state-provider handle before a worker refreshes it, bounding
/// read-transaction lifetime and snapshot staleness while amortizing provider setup across a
/// drained batch.
const MAX_READS_PER_PROVIDER: usize = 128;

/// Maximum number of prefetch worker threads a pool will spawn. Past roughly this point extra
/// workers add OS threads and MDBX read slots without increasing `NVMe` queue-depth utilization.
pub const MAX_PREFETCH_WORKERS: usize = 256;

/// Pool of OS threads that read hinted state through independent state-provider handles.
///
/// Each hinted request is read once at the latest state and the value discarded: the read exists
/// solely to fault the corresponding database pages into the OS page cache concurrently, ahead of
/// the serial journaled reads that follow during execution. A slightly stale view is fine — the
/// pages are the same either way, and the metered read path is untouched.
///
/// Once installed as the process-wide prefetcher the pool is never dropped, so its worker
/// threads live until process exit; [`Self::join`] exists for owners that want a graceful
/// drain-and-shutdown (tests, tools).
#[derive(Debug)]
pub struct StatePrefetchPool {
    senders: Vec<mpsc::SyncSender<PrefetchRequest>>,
    workers: Vec<thread::JoinHandle<()>>,
    next_worker: AtomicUsize,
    queued_requests: Arc<AtomicUsize>,
}

impl StatePrefetchPool {
    /// Spawns `workers` prefetch threads reading the latest state from `provider`.
    ///
    /// # Panics
    ///
    /// Panics if `workers` is zero or a worker thread cannot be spawned.
    pub fn spawn<P>(provider: P, workers: usize) -> Self
    where
        P: StateProviderFactory + Clone + Send + Sync + 'static,
    {
        assert!(workers > 0, "prefetch pool requires at least one worker");
        let mut senders = Vec::with_capacity(workers);
        let mut handles = Vec::with_capacity(workers);
        let queued_requests = Arc::new(AtomicUsize::new(0));
        let worker_queue_capacity = MAX_QUEUED_REQUESTS.div_ceil(workers);
        for index in 0..workers {
            let (sender, receiver) = mpsc::sync_channel(worker_queue_capacity);
            let provider = provider.clone();
            let queued_requests = Arc::clone(&queued_requests);
            let handle = thread::Builder::new()
                .name(format!("state-prefetch-{index}"))
                .spawn(move || Self::worker_loop(provider, receiver, queued_requests))
                .expect("failed to spawn state prefetch worker");
            senders.push(sender);
            handles.push(handle);
        }
        Self { senders, workers: handles, next_worker: AtomicUsize::new(0), queued_requests }
    }

    /// Drains all queued hints and waits for every worker to exit.
    pub fn join(mut self) {
        self.senders.clear();
        for handle in self.workers.drain(..) {
            handle.join().expect("state prefetch worker panicked");
        }
    }

    /// Reads hinted state until every sender is dropped.
    ///
    /// One state-provider handle is amortized across each drained batch instead of created
    /// per request, capped at [`MAX_READS_PER_PROVIDER`] so a busy queue can neither pin a
    /// long-lived read transaction nor serve arbitrarily stale snapshots.
    fn worker_loop<P: StateProviderFactory>(
        provider: P,
        receiver: mpsc::Receiver<PrefetchRequest>,
        queued_requests: Arc<AtomicUsize>,
    ) {
        while let Ok(request) = receiver.recv() {
            queued_requests.fetch_sub(1, Ordering::Relaxed);
            let state = match provider.latest() {
                Ok(state) => state,
                Err(error) => {
                    // The dequeued request is charged to read_errors_total, preserving
                    // requests_enqueued_total == read_seconds count + read_errors_total.
                    PrefetchMetrics::read_errors_total().increment(1);
                    trace!(error = %error, request = ?request, "prefetch state provider unavailable");
                    continue;
                }
            };
            Self::read(&*state, request);
            for _ in 1..MAX_READS_PER_PROVIDER {
                match receiver.try_recv() {
                    Ok(request) => {
                        queued_requests.fetch_sub(1, Ordering::Relaxed);
                        Self::read(&*state, request);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    /// Performs one hinted read and discards the value.
    fn read<S: StateProvider + ?Sized>(state: &S, request: PrefetchRequest) {
        let started = Instant::now();
        let (kind, result) = match request {
            PrefetchRequest::Slot { address, slot } => {
                ("slot", state.storage(address, B256::from(slot)).map(|_| ()))
            }
            PrefetchRequest::Account { address } => {
                ("account", state.basic_account(&address).map(|_| ()))
            }
            // `account_code` resolves the account first, warming both the account entry and
            // its bytecode.
            PrefetchRequest::Code { address } => ("code", state.account_code(&address).map(|_| ())),
        };
        match result {
            Ok(()) => PrefetchMetrics::read_seconds(kind).record(started.elapsed()),
            Err(error) => {
                PrefetchMetrics::read_errors_total().increment(1);
                trace!(error = %error, request = ?request, "prefetch read failed");
            }
        }
    }
}

impl StatePrefetcher for StatePrefetchPool {
    fn prefetch(&self, requests: &[PrefetchRequest]) {
        PrefetchMetrics::hints_total().increment(1);
        for &request in requests {
            if !self.try_reserve_queue_slot() {
                PrefetchMetrics::requests_dropped_total().increment(1);
                continue;
            }
            let index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.senders.len();
            match self.senders[index].try_send(request) {
                Ok(()) => PrefetchMetrics::requests_enqueued_total().increment(1),
                Err(_) => {
                    self.queued_requests.fetch_sub(1, Ordering::Relaxed);
                    PrefetchMetrics::requests_dropped_total().increment(1);
                }
            }
        }
    }
}

impl StatePrefetchPool {
    /// Reserves one of the globally bounded queue entries without blocking a producer.
    fn try_reserve_queue_slot(&self) -> bool {
        let mut queued = self.queued_requests.load(Ordering::Relaxed);
        while queued < MAX_QUEUED_REQUESTS {
            match self.queued_requests.compare_exchange_weak(
                queued,
                queued + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(current) => queued = current,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use reth_provider::test_utils::MockEthProvider;

    use super::*;

    #[test]
    fn queue_admission_is_globally_bounded() {
        let pool = StatePrefetchPool {
            senders: Vec::new(),
            workers: Vec::new(),
            next_worker: AtomicUsize::new(0),
            queued_requests: Arc::new(AtomicUsize::new(0)),
        };

        for _ in 0..MAX_QUEUED_REQUESTS {
            assert!(pool.try_reserve_queue_slot());
        }
        assert!(!pool.try_reserve_queue_slot());
    }

    #[test]
    fn drains_all_hinted_requests_and_exits_cleanly() {
        let pool = StatePrefetchPool::spawn(MockEthProvider::default(), 4);
        let address = Address::repeat_byte(0x01);
        let requests: Vec<PrefetchRequest> = (0..64u64)
            .map(|slot| PrefetchRequest::Slot { address, slot: U256::from(slot) })
            .chain([
                PrefetchRequest::Account { address },
                PrefetchRequest::Code { address: Address::repeat_byte(0x02) },
            ])
            .collect();
        pool.prefetch(&requests);
        pool.prefetch(&requests[..5]);
        // Join blocks until every queued read completed; a stuck or panicked
        // worker fails the test.
        pool.join();
    }
}
