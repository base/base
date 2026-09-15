//! Worker pool that resolves state prefetch hints against the live state provider.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use alloy_primitives::B256;
use base_precompile_storage::{PrefetchRequest, StatePrefetcher};
use reth_provider::StateProviderFactory;
use tracing::trace;

/// Per-worker queue capacity. Overflow drops the hint rather than blocking the
/// execution path that produced it.
const WORKER_QUEUE_CAPACITY: usize = 1024;

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
        for index in 0..workers {
            let (sender, receiver) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
            let provider = provider.clone();
            let handle = thread::Builder::new()
                .name(format!("state-prefetch-{index}"))
                .spawn(move || Self::worker_loop(provider, receiver))
                .expect("failed to spawn state prefetch worker");
            senders.push(sender);
            handles.push(handle);
        }
        Self { senders, workers: handles, next_worker: AtomicUsize::new(0) }
    }

    /// Drains all queued hints and waits for every worker to exit.
    pub fn join(mut self) {
        self.senders.clear();
        for handle in self.workers.drain(..) {
            handle.join().expect("state prefetch worker panicked");
        }
    }

    /// Reads hinted state until every sender is dropped.
    fn worker_loop<P: StateProviderFactory>(
        provider: P,
        receiver: mpsc::Receiver<PrefetchRequest>,
    ) {
        while let Ok(request) = receiver.recv() {
            let result = provider.latest().and_then(|state| {
                state.storage(request.address, B256::from(request.slot)).map(|_| ())
            });
            if let Err(error) = result {
                trace!(
                    error = %error,
                    address = %request.address,
                    slot = %request.slot,
                    "prefetch read failed"
                );
            }
        }
    }
}

impl StatePrefetcher for StatePrefetchPool {
    fn prefetch(&self, requests: &[PrefetchRequest]) {
        for &request in requests {
            let index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.senders.len();
            let _ = self.senders[index].try_send(request);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use reth_provider::test_utils::MockEthProvider;

    use super::*;

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
