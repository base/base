//! Worker pool that resolves state prefetch hints against the live state provider.

use std::{
    sync::{
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

/// Per-worker queue capacity. Sized to absorb bursts of hint batches without ever blocking the
/// execution path that produced them; overflow drops the hint (the journaled read still happens,
/// it just pays the fault itself).
const WORKER_QUEUE_CAPACITY: usize = 1024;

/// Maximum reads served by one state-provider handle before a worker refreshes it, bounding
/// read-transaction lifetime and snapshot staleness while amortizing provider setup across a
/// drained batch.
const MAX_READS_PER_PROVIDER: usize = 128;

/// Pool of OS threads that read hinted state through independent state-provider handles.
///
/// Each hinted request is read once at the latest state and the value discarded: the read exists
/// solely to fault the corresponding database pages into the OS page cache concurrently, ahead of
/// the serial journaled reads that follow during execution. A slightly stale view is fine — the
/// pages are the same either way, and the metered read path is untouched.
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
    ///
    /// One state-provider handle is amortized across each drained batch instead of created
    /// per request, capped at [`MAX_READS_PER_PROVIDER`] so a busy queue can neither pin a
    /// long-lived read transaction nor serve arbitrarily stale snapshots.
    fn worker_loop<P: StateProviderFactory>(
        provider: P,
        receiver: mpsc::Receiver<PrefetchRequest>,
    ) {
        while let Ok(request) = receiver.recv() {
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
                    Ok(request) => Self::read(&*state, request),
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
            let index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.senders.len();
            match self.senders[index].try_send(request) {
                Ok(()) => PrefetchMetrics::requests_enqueued_total().increment(1),
                Err(_) => PrefetchMetrics::requests_dropped_total().increment(1),
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
