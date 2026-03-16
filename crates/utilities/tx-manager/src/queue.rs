//! Bounded async transaction send queue with backpressure.
//!
//! [`TxQueue`] wraps a [`TxManager`] and limits the number of in-flight
//! transactions using a [`tokio::sync::Semaphore`]. Nonces are assigned in
//! queue order because [`TxManager::send_async`] is called on the caller's
//! task before spawning a background completion task.

use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::{debug, warn};

use crate::{SendResponse, TxCandidate, TxManager};

/// Pairs a caller-supplied identifier with the outcome of a transaction send.
#[derive(Debug)]
pub struct SendResult<T> {
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
///
/// `Clone` produces a handle to the same queue — clones share the semaphore and
/// underlying [`TxManager`], so backpressure limits apply across all handles.
#[derive(Debug, Clone)]
pub struct TxQueue<M> {
    tx_mgr: Arc<M>,
    semaphore: Arc<Semaphore>,
}

impl<M: TxManager + 'static> TxQueue<M> {
    /// Creates a new `TxQueue`.
    ///
    /// `max_pending` controls how many transactions may be in-flight at once.
    /// `None` means unlimited (no backpressure).
    pub fn new(tx_mgr: Arc<M>, max_pending: Option<NonZeroUsize>) -> Self {
        let permits = max_pending.map(|n| n.get()).unwrap_or(Semaphore::MAX_PERMITS);
        debug!(max_pending = permits, "tx queue created");
        Self { tx_mgr, semaphore: Arc::new(Semaphore::new(permits)) }
    }

    /// Queues a transaction for sending, blocking if the queue is full.
    ///
    /// 1. Acquires a semaphore permit (blocks when at capacity).
    /// 2. Calls [`TxManager::send_async`] **on the caller's task** so that
    ///    nonces are reserved in call order.
    /// 3. Spawns a background task that awaits the [`crate::SendHandle`],
    ///    delivers the result on `result_tx`, and releases the permit.
    ///
    /// # Warning: not cancellation-safe
    ///
    /// This future **must not** be dropped after polling begins — do not use
    /// it inside `tokio::select!`, `tokio::time::timeout`,
    /// `FuturesUnordered`, or any combinator that may drop in-progress
    /// futures. If dropped after [`TxManager::send_async`] has been called
    /// but before the background task is spawned, a nonce is consumed and
    /// the result will never be delivered to `result_tx`. The underlying
    /// transaction task will still run to completion (sign, publish, and
    /// poll), wasting gas on a transaction no caller is awaiting.
    pub async fn send<T: Send + 'static>(
        &self,
        id: T,
        candidate: TxCandidate,
        result_tx: mpsc::Sender<SendResult<T>>,
    ) {
        debug!(available = self.semaphore.available_permits(), "acquiring send permit",);
        let permit =
            Arc::clone(&self.semaphore).acquire_owned().await.expect("semaphore is never closed");
        debug!("send permit acquired");
        self.dispatch(permit, id, candidate, result_tx).await;
    }

    /// Attempts to queue a transaction if a permit is available.
    ///
    /// Returns `Ok(())` if a permit was available and the transaction was
    /// enqueued. Returns `Err((id, candidate))` if the queue is full, giving
    /// ownership of the consumed values back to the caller.
    ///
    /// # Warning: not cancellation-safe
    ///
    /// This future **must not** be dropped after polling begins — do not use
    /// it inside `tokio::select!`, `tokio::time::timeout`,
    /// `FuturesUnordered`, or any combinator that may drop in-progress
    /// futures. If dropped after [`TxManager::send_async`] has been called
    /// but before the background task is spawned, a nonce is consumed and
    /// the result will never be delivered to `result_tx`. The underlying
    /// transaction task will still run to completion (sign, publish, and
    /// poll), wasting gas on a transaction no caller is awaiting.
    pub async fn try_send<T: Send + 'static>(
        &self,
        id: T,
        candidate: TxCandidate,
        result_tx: mpsc::Sender<SendResult<T>>,
    ) -> Result<(), (T, TxCandidate)> {
        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!(
                    available = self.semaphore.available_permits(),
                    "try_send rejected, queue full",
                );
                return Err((id, candidate));
            }
        };
        self.dispatch(permit, id, candidate, result_tx).await;
        Ok(())
    }

    /// Reserves a nonce on the caller's task, then spawns a background task to
    /// await the send and deliver the receipt.
    async fn dispatch<T: Send + 'static>(
        &self,
        permit: OwnedSemaphorePermit,
        id: T,
        candidate: TxCandidate,
        result_tx: mpsc::Sender<SendResult<T>>,
    ) {
        debug!("dispatching send_async");
        let handle = self.tx_mgr.send_async(candidate).await;
        tokio::spawn(async move {
            let result = handle.await;
            let success = result.is_ok();
            // Release the permit before sending the result to avoid deadlock:
            // the result channel may be full, blocking this task while holding
            // a permit that the receiver needs freed before it can drain.
            drop(permit);
            if result_tx.send(SendResult { id, result }).await.is_err() {
                warn!(success, "result receiver dropped, tx receipt lost");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use alloy_primitives::Address;
    use tokio::sync::{Notify, mpsc};

    use super::*;
    use crate::{SendHandle, TxManagerError, test_utils::stub_receipt};

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

    // ── ErrorMockTxManager ───────────────────────────────────────────────

    /// A mock whose `send_async` always resolves to an error.
    #[derive(Debug)]
    struct ErrorMockTxManager;

    impl TxManager for ErrorMockTxManager {
        async fn send(&self, _candidate: TxCandidate) -> SendResponse {
            Err(TxManagerError::NonceTooLow)
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tx.send(Err(TxManagerError::NonceTooLow)).expect("receiver not dropped");
            SendHandle::new(rx)
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    async fn recv_timeout<T>(rx: &mut mpsc::Receiver<T>, secs: u64) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(secs), rx.recv())
            .await
            .expect("should receive within timeout")
            .expect("channel should not be closed")
    }

    // ── Tests ───────────────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn send_blocks_when_full() {
        let mgr = Arc::new(GatedMockTxManager::new(3));
        let (result_tx, mut result_rx) = mpsc::channel::<SendResult<u64>>(16);

        let queue = Arc::new(TxQueue::new(Arc::clone(&mgr), Some(NonZeroUsize::new(2).unwrap())));

        // First two sends should proceed immediately.
        queue.send(1, stub_candidate(), result_tx.clone()).await;
        queue.send(2, stub_candidate(), result_tx.clone()).await;

        // Third send should block because both permits are held.
        let queue_clone = Arc::clone(&queue);
        let result_tx_clone = result_tx.clone();
        let mut blocked = tokio::spawn(async move {
            // This will not return until a permit is freed.
            queue_clone.send(3, stub_candidate(), result_tx_clone).await;
        });

        // On current_thread, yield_now deterministically advances the spawned
        // task to its semaphore await. Use a short timeout as a safety net.
        let timeout_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut blocked).await;
        assert!(timeout_result.is_err(), "third send should be blocked");

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

    #[tokio::test(flavor = "current_thread")]
    async fn try_send_returns_err_when_full() {
        let mgr = Arc::new(GatedMockTxManager::new(3));
        let (result_tx, mut result_rx) = mpsc::channel::<SendResult<u64>>(16);

        let queue = TxQueue::new(Arc::clone(&mgr), Some(NonZeroUsize::new(2).unwrap()));

        assert!(queue.try_send(1, stub_candidate(), result_tx.clone()).await.is_ok());
        assert!(queue.try_send(2, stub_candidate(), result_tx.clone()).await.is_ok());

        // Queue is full — should return the id and candidate back.
        let err = queue.try_send(3, stub_candidate(), result_tx.clone()).await;
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().0, 3);

        // Free a slot.
        mgr.complete(0);
        let _ = result_rx.recv().await;

        // The permit was released before the result was sent (see `dispatch`),
        // so a slot is guaranteed to be free by the time we receive the result.
        assert!(queue.try_send(4, stub_candidate(), result_tx.clone()).await.is_ok());

        // Cleanup.
        mgr.complete(1);
        mgr.complete(2);
    }

    #[tokio::test]
    async fn results_delivered_with_matching_ids() {
        let mgr = Arc::new(MockTxManager::new());
        let (result_tx, mut result_rx) = mpsc::channel::<SendResult<String>>(16);

        let queue = TxQueue::new(mgr, Some(NonZeroUsize::new(10).unwrap()));

        let ids = ["alpha", "beta", "gamma"];
        for id in &ids {
            queue.send(id.to_string(), stub_candidate(), result_tx.clone()).await;
        }

        let mut received = Vec::new();
        for _ in 0..3 {
            let receipt = recv_timeout(&mut result_rx, 2).await;
            assert!(receipt.result.is_ok());
            received.push(receipt.id);
        }

        received.sort();
        let mut expected: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn max_pending_none_means_unlimited() {
        let mgr = Arc::new(MockTxManager::new());
        let (result_tx, mut result_rx) = mpsc::channel::<SendResult<u64>>(256);

        let queue = TxQueue::new(Arc::clone(&mgr), None);

        for i in 0..100 {
            queue.send(i, stub_candidate(), result_tx.clone()).await;
        }

        for _ in 0..100 {
            let receipt = recv_timeout(&mut result_rx, 5).await;
            assert!(receipt.result.is_ok());
        }

        assert_eq!(mgr.call_count.load(Ordering::SeqCst), 100);
    }

    #[tokio::test]
    async fn error_result_propagates_and_releases_permit() {
        let mgr = Arc::new(ErrorMockTxManager);
        let (result_tx, mut result_rx) = mpsc::channel::<SendResult<u64>>(16);

        // Only one permit — if the error doesn't release it, the second send hangs.
        let queue = TxQueue::new(Arc::clone(&mgr), Some(NonZeroUsize::new(1).unwrap()));

        // First send: should resolve to an error.
        queue.send(1, stub_candidate(), result_tx.clone()).await;
        let first = recv_timeout(&mut result_rx, 2).await;
        assert_eq!(first.id, 1);
        assert_eq!(first.result.unwrap_err(), TxManagerError::NonceTooLow);

        // Second send on the SAME queue: proves the permit was released despite
        // the error. With only one permit, this would hang if the first send
        // leaked it.
        queue.send(2, stub_candidate(), result_tx.clone()).await;
        let second = recv_timeout(&mut result_rx, 2).await;
        assert_eq!(second.id, 2);
        assert_eq!(second.result.unwrap_err(), TxManagerError::NonceTooLow);
    }
}
