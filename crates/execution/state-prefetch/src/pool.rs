//! Worker pool that resolves state prefetch hints against the live state provider.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use alloy_primitives::B256;
use base_precompile_storage::{PrefetchRequest, StatePrefetcher};
use reth_provider::{StateProvider, StateProviderFactory};
use tracing::trace;

/// Global cap for queued requests across all workers. Increasing worker count
/// does not increase the maximum retained queue memory.
const MAX_QUEUED_REQUESTS: usize = 4_096;

/// Maximum reads served by one state-provider handle before a worker refreshes it.
const MAX_READS_PER_PROVIDER: usize = 128;

/// Maximum number of prefetch worker threads a pool will spawn.
pub const MAX_PREFETCH_WORKERS: usize = 256;

/// Pool of OS threads that read hinted state through independent state-provider handles.
///
/// Each hinted request is read once at the latest state and the value discarded. Once installed
/// as the process-wide prefetcher the pool is never dropped, so its workers live until process
/// exit; [`Self::join`] exists for owners that want a graceful drain and shutdown.
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
    /// One state-provider handle is amortized across each drained batch, capped so a busy queue
    /// cannot pin a long-lived read transaction or serve an arbitrarily stale snapshot.
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
        if let Err(error) = state.storage(request.address, B256::from(request.slot)) {
            trace!(
                error = %error,
                address = %request.address,
                slot = %request.slot,
                "prefetch read failed"
            );
        }
    }

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

impl StatePrefetcher for StatePrefetchPool {
    fn prefetch(&self, requests: &[PrefetchRequest]) {
        for &request in requests {
            if !self.try_reserve_queue_slot() {
                continue;
            }
            let index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.senders.len();
            if self.senders[index].try_send(request).is_err() {
                self.queued_requests.fetch_sub(1, Ordering::Relaxed);
            }
        }
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
        let requests: Vec<PrefetchRequest> =
            (0..64u64).map(|slot| PrefetchRequest { address, slot: U256::from(slot) }).collect();
        pool.prefetch(&requests);
        pool.prefetch(&requests[..5]);
        pool.join();
    }
}
