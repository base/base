use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Handle to trigger graceful engine shutdown.
///
/// This handle can be used to request a graceful shutdown of the engine,
/// which will persist all remaining in-memory blocks before terminating.
#[derive(Clone, Debug)]
pub struct EngineShutdown {
    /// Channel to send shutdown signal.
    tx: Arc<Mutex<Option<oneshot::Sender<EngineShutdownRequest>>>>,
}

impl EngineShutdown {
    /// Creates a new [`EngineShutdown`] handle and returns the receiver.
    pub fn new() -> (Self, oneshot::Receiver<EngineShutdownRequest>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx: Arc::new(Mutex::new(Some(tx))) }, rx)
    }

    /// Requests a graceful engine shutdown.
    ///
    /// All remaining in-memory blocks will be persisted before the engine terminates.
    ///
    /// Returns a receiver that resolves when shutdown is complete.
    /// Returns `None` if shutdown was already triggered.
    pub fn shutdown(&self) -> Option<oneshot::Receiver<()>> {
        let mut guard = self.tx.lock();
        let tx = guard.take()?;
        let (done_tx, done_rx) = oneshot::channel();
        let _ = tx.send(EngineShutdownRequest { done_tx });
        Some(done_rx)
    }
}

impl Default for EngineShutdown {
    fn default() -> Self {
        Self { tx: Arc::new(Mutex::new(None)) }
    }
}

/// Request to shutdown the engine.
#[derive(Debug)]
pub struct EngineShutdownRequest {
    /// Channel to signal shutdown completion.
    pub done_tx: oneshot::Sender<()>,
}

#[cfg(test)]
mod tests {
    use super::EngineShutdown;

    #[tokio::test]
    async fn shutdown_is_shared_and_waits_for_persistence() {
        let (shutdown, request) = EngineShutdown::new();
        let clone = shutdown.clone();
        let mut completion = shutdown.shutdown().expect("first shutdown sends request");
        assert!(clone.shutdown().is_none());
        let request = request.await.expect("driver receives shutdown");
        assert!(matches!(
            completion.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        request.done_tx.send(()).expect("caller still awaits persistence");
        completion.await.expect("driver acknowledged persistence");
    }
}
