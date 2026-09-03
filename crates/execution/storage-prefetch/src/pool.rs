//! Worker pool that resolves storage prefetch hints against the live state provider.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use alloy_primitives::{Address, B256, U256};
use base_precompile_storage::StoragePrefetcher;
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

/// Pool of OS threads that read hinted storage slots through independent state-provider handles.
///
/// Each hinted slot is read once at the latest state and the value discarded: the read exists
/// solely to fault the slot's database pages into the OS page cache concurrently, ahead of the
/// serial journaled reads that follow during execution. A slightly stale view is fine — the
/// slot's pages are the same either way, and the metered read path is untouched.
#[derive(Debug)]
pub struct StoragePrefetchPool {
    senders: Vec<mpsc::SyncSender<(Address, U256)>>,
    workers: Vec<thread::JoinHandle<()>>,
    next_worker: AtomicUsize,
}

impl StoragePrefetchPool {
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
                .name(format!("storage-prefetch-{index}"))
                .spawn(move || Self::worker_loop(provider, receiver))
                .expect("failed to spawn storage prefetch worker");
            senders.push(sender);
            handles.push(handle);
        }
        Self { senders, workers: handles, next_worker: AtomicUsize::new(0) }
    }

    /// Drains all queued hints and waits for every worker to exit.
    pub fn join(mut self) {
        self.senders.clear();
        for handle in self.workers.drain(..) {
            handle.join().expect("storage prefetch worker panicked");
        }
    }

    /// Reads hinted slots until every sender is dropped.
    ///
    /// One state-provider handle is amortized across each drained batch instead of created
    /// per slot, capped at [`MAX_READS_PER_PROVIDER`] so a busy queue can neither pin a
    /// long-lived read transaction nor serve arbitrarily stale snapshots.
    fn worker_loop<P: StateProviderFactory>(
        provider: P,
        receiver: mpsc::Receiver<(Address, U256)>,
    ) {
        while let Ok(request) = receiver.recv() {
            let state = match provider.latest() {
                Ok(state) => state,
                Err(error) => {
                    // The dequeued request is charged to read_errors_total, preserving
                    // slots_enqueued_total == read_seconds count + read_errors_total.
                    PrefetchMetrics::read_errors_total().increment(1);
                    trace!(
                        error = %error,
                        address = %request.0,
                        "prefetch state provider unavailable"
                    );
                    continue;
                }
            };
            Self::read_slot(&*state, request);
            for _ in 1..MAX_READS_PER_PROVIDER {
                match receiver.try_recv() {
                    Ok(request) => Self::read_slot(&*state, request),
                    Err(_) => break,
                }
            }
        }
    }

    /// Reads one hinted slot and discards the value.
    fn read_slot<S: StateProvider + ?Sized>(state: &S, (address, slot): (Address, U256)) {
        let started = Instant::now();
        match state.storage(address, B256::from(slot)) {
            Ok(_) => PrefetchMetrics::read_seconds().record(started.elapsed()),
            Err(error) => {
                PrefetchMetrics::read_errors_total().increment(1);
                trace!(error = %error, address = %address, "prefetch read failed");
            }
        }
    }
}

impl StoragePrefetcher for StoragePrefetchPool {
    fn prefetch(&self, address: Address, slots: &[U256]) {
        PrefetchMetrics::hints_total().increment(1);
        for &slot in slots {
            let index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.senders.len();
            match self.senders[index].try_send((address, slot)) {
                Ok(()) => PrefetchMetrics::slots_enqueued_total().increment(1),
                Err(_) => PrefetchMetrics::slots_dropped_total().increment(1),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reth_provider::test_utils::MockEthProvider;

    use super::*;

    #[test]
    fn drains_all_hinted_slots_and_exits_cleanly() {
        let pool = StoragePrefetchPool::spawn(MockEthProvider::default(), 4);
        let address = Address::repeat_byte(0x01);
        let slots: Vec<U256> = (0..64u64).map(U256::from).collect();
        pool.prefetch(address, &slots);
        pool.prefetch(Address::repeat_byte(0x02), &slots[..5]);
        // Join blocks until every queued read completed; a stuck or panicked
        // worker fails the test.
        pool.join();
    }
}
