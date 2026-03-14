//! Bounded async transaction send queue with backpressure.
//!
//! [`TxQueue`] wraps a [`TxManager`] and limits the number of in-flight
//! transactions using a [`tokio::sync::Semaphore`]. Nonces are assigned in
//! queue order because [`TxManager::send_async`] is called on the caller's
//! task before spawning a background completion task.

use std::sync::Arc;

use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tracing::debug;

use crate::{SendResponse, TxCandidate, TxManager};

/// Pairs a caller-supplied identifier with the outcome of a transaction send.
#[derive(Debug)]
pub struct TxReceipt<T> {
    /// Caller-supplied identifier for the transaction.
    pub id: T,
    /// The result of the transaction send.
    pub result: SendResponse,
}

/// Bounded async transaction send queue with backpressure.
///
/// Limits the number of concurrently in-flight transactions via a semaphore.
/// Nonces are assigned in call order because [`TxManager::send_async`] runs on
/// the caller's task before the background completion task is spawned.
#[derive(Debug)]
pub struct TxQueue<M> {
    tx_mgr: Arc<M>,
    semaphore: Arc<Semaphore>,
}

impl<M: TxManager + 'static> TxQueue<M> {
    /// Creates a new `TxQueue`.
    ///
    /// `max_pending` controls how many transactions may be in-flight at once.
    /// A value of `0` means unlimited (no backpressure).
    pub fn new(tx_mgr: Arc<M>, max_pending: usize) -> Self {
        let permits = if max_pending == 0 { Semaphore::MAX_PERMITS } else { max_pending };
        debug!(max_pending = permits, "tx queue created");
        Self { tx_mgr, semaphore: Arc::new(Semaphore::new(permits)) }
    }

    /// Queues a transaction for sending, blocking if the queue is full.
    ///
    /// 1. Acquires a semaphore permit (blocks when at capacity).
    /// 2. Calls [`TxManager::send_async`] **on the caller's task** so that
    ///    nonces are reserved in call order.
    /// 3. Spawns a background task that awaits the [`SendHandle`], delivers the
    ///    result on `result_tx`, and releases the permit.
    pub async fn send<T: Send + 'static>(
        &self,
        id: T,
        candidate: TxCandidate,
        result_tx: mpsc::Sender<TxReceipt<T>>,
    ) {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        self.dispatch(permit, id, candidate, result_tx).await;
    }

    /// Attempts to queue a transaction without blocking.
    ///
    /// Returns `true` if a permit was available and the transaction was
    /// enqueued, `false` if the queue is full.
    pub async fn try_send<T: Send + 'static>(
        &self,
        id: T,
        candidate: TxCandidate,
        result_tx: mpsc::Sender<TxReceipt<T>>,
    ) -> bool {
        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.dispatch(permit, id, candidate, result_tx).await;
        true
    }

    /// Reserves a nonce on the caller's task, then spawns a background task to
    /// await the send and deliver the receipt.
    async fn dispatch<T: Send + 'static>(
        &self,
        permit: OwnedSemaphorePermit,
        id: T,
        candidate: TxCandidate,
        result_tx: mpsc::Sender<TxReceipt<T>>,
    ) {
        let handle = self.tx_mgr.send_async(candidate).await;
        tokio::spawn(async move {
            let result = handle.await;
            if result_tx.send(TxReceipt { id, result }).await.is_err() {
                debug!("result receiver dropped, tx receipt lost");
            }
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    use alloy_primitives::Address;
    use tokio::sync::{mpsc, Notify};

    use super::*;
    use crate::{test_utils::stub_receipt, SendHandle};

    fn stub_candidate() -> TxCandidate {
        TxCandidate::default()
    }

    // ── MockTxManager ───────────────────────────────────────────────────

    /// A mock that immediately resolves the `SendHandle` with a stub receipt.
    #[derive(Debug)]
    struct MockTxManager {
        call_count: AtomicU64,
    }

    impl MockTxManager {
        fn new() -> Self {
            Self { call_count: AtomicU64::new(0) }
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = tokio::sync::oneshot::channel();
            tx.send(Ok(stub_receipt())).expect("receiver not dropped");
            SendHandle::new(rx)
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    // ── GatedMockTxManager ──────────────────────────────────────────────

    /// A mock whose `SendHandle` blocks until the test calls `complete(index)`.
    #[derive(Debug)]
    struct GatedMockTxManager {
        call_count: AtomicU64,
        /// One `Notify` per expected send — test calls `complete(i)` to
        /// unblock the i-th send.
        gates: Vec<Arc<Notify>>,
    }

    impl GatedMockTxManager {
        fn new(num_gates: usize) -> Self {
            let gates = (0..num_gates).map(|_| Arc::new(Notify::new())).collect();
            Self { call_count: AtomicU64::new(0), gates }
        }

        /// Unblocks the `index`-th `send_async` call.
        fn complete(&self, index: usize) {
            self.gates[index].notify_one();
        }
    }

    impl TxManager for GatedMockTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
            let gate = Arc::clone(&self.gates[idx]);
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                gate.notified().await;
                let _ = tx.send(Ok(stub_receipt()));
            });
            SendHandle::new(rx)
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    // ── Tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn send_blocks_when_full() {
        let mgr = Arc::new(GatedMockTxManager::new(3));
        let (result_tx, mut result_rx) = mpsc::channel::<TxReceipt<u64>>(16);

        let queue = Arc::new(TxQueue::new(Arc::clone(&mgr), 2));

        // First two sends should proceed immediately.
        queue.send(1, stub_candidate(), result_tx.clone()).await;
        queue.send(2, stub_candidate(), result_tx.clone()).await;

        // Third send should block because both permits are held.
        let queue_clone = Arc::clone(&queue);
        let result_tx_clone = result_tx.clone();
        let blocked = tokio::spawn(async move {
            // This will not return until a permit is freed.
            queue_clone.send(3, stub_candidate(), result_tx_clone).await;
        });

        // Give the spawned task time to reach the semaphore.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!blocked.is_finished(), "third send should be blocked");

        // Complete the first transaction — frees a permit.
        mgr.complete(0);
        let _ = result_rx.recv().await;

        // Now the third send should unblock.
        tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
            .await
            .expect("third send should unblock after permit freed")
            .expect("task should not panic");

        // Cleanup: complete remaining.
        mgr.complete(1);
        mgr.complete(2);
    }

    #[tokio::test]
    async fn try_send_returns_false_when_full() {
        let mgr = Arc::new(GatedMockTxManager::new(4));
        let (result_tx, mut result_rx) = mpsc::channel::<TxReceipt<u64>>(16);

        let queue = TxQueue::new(Arc::clone(&mgr), 2);

        assert!(queue.try_send(1, stub_candidate(), result_tx.clone()).await);
        assert!(queue.try_send(2, stub_candidate(), result_tx.clone()).await);
        assert!(!queue.try_send(3, stub_candidate(), result_tx.clone()).await);

        // Free a slot.
        mgr.complete(0);
        let _ = result_rx.recv().await;

        // Allow the permit to be released by the spawned task.
        tokio::task::yield_now().await;

        assert!(queue.try_send(4, stub_candidate(), result_tx.clone()).await);

        // Cleanup.
        mgr.complete(1);
        mgr.complete(2);
    }

    #[tokio::test]
    async fn results_delivered_with_matching_ids() {
        let mgr = Arc::new(MockTxManager::new());
        let (result_tx, mut result_rx) = mpsc::channel::<TxReceipt<String>>(16);

        let queue = TxQueue::new(mgr, 10);

        let ids = ["alpha", "beta", "gamma"];
        for id in &ids {
            queue.send(id.to_string(), stub_candidate(), result_tx.clone()).await;
        }

        let mut received = Vec::new();
        for _ in 0..3 {
            let receipt = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                result_rx.recv(),
            )
            .await
            .expect("should receive within timeout")
            .expect("channel should not be closed");

            assert!(receipt.result.is_ok());
            received.push(receipt.id);
        }

        received.sort();
        let mut expected: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn max_pending_zero_means_unlimited() {
        let mgr = Arc::new(MockTxManager::new());
        let (result_tx, mut result_rx) = mpsc::channel::<TxReceipt<u64>>(256);

        let queue = TxQueue::new(Arc::clone(&mgr), 0);

        for i in 0..100 {
            queue.send(i, stub_candidate(), result_tx.clone()).await;
        }

        for _ in 0..100 {
            let receipt = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                result_rx.recv(),
            )
            .await
            .expect("should receive within timeout")
            .expect("channel should not be closed");
            assert!(receipt.result.is_ok());
        }

        assert_eq!(mgr.call_count.load(Ordering::SeqCst), 100);
    }
}
