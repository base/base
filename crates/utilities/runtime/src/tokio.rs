//! Production runtime backed by Tokio.

use std::{future::Future, pin::Pin, time::Duration};

use futures::{StreamExt, stream::BoxStream};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;

use crate::{Cancellation, Clock, Spawner, TaskError, TaskHandle};

/// Production runtime backed by `tokio`.
///
/// Wraps `tokio::time` for clock operations, `tokio::spawn` for task
/// spawning, and `tokio_util::sync::CancellationToken` for cancellation.
///
/// Create one instance per logical service and pass it (cloned as needed)
/// into components like `BatchDriver` and `HybridBlockSource`. All clones
/// share the same cancellation scope; call [`cancel`](Cancellation::cancel)
/// on any clone to shut down all components that share this runtime.
#[derive(Clone, Debug)]
pub struct TokioRuntime {
    token: CancellationToken,
    epoch: std::time::Instant,
}

impl TokioRuntime {
    /// Create a new runtime with a fresh cancellation scope.
    pub fn new() -> Self {
        Self { token: CancellationToken::new(), epoch: std::time::Instant::now() }
    }

    /// Wrap an existing `CancellationToken`.
    ///
    /// Useful when migrating code that already creates tokens externally.
    /// The `now()` epoch is set to the current wall-clock instant.
    pub fn with_token(token: CancellationToken) -> Self {
        Self { token, epoch: std::time::Instant::now() }
    }

    /// Expose the inner `CancellationToken` for interop with code that has
    /// not yet been migrated to accept `R: Runtime`.
    pub const fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Default for TokioRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioRuntime {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn interval(&self, period: Duration) -> BoxStream<'static, ()> {
        IntervalStream::new(tokio::time::interval(period)).map(|_| ()).boxed()
    }
}

impl Spawner for TokioRuntime {
    fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = tokio::spawn(future);
        TaskHandle::new(async move {
            handle.await.map_err(|e| {
                if e.is_cancelled() { TaskError::Cancelled } else { TaskError::Panicked }
            })
        })
    }
}

impl Cancellation for TokioRuntime {
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // cancelled_owned() takes ownership of a cloned token, producing a
        // 'static future that does not borrow self.
        Box::pin(self.token.clone().cancelled_owned())
    }

    fn cancel(&self) {
        self.token.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    fn child(&self) -> Self {
        Self { token: self.token.child_token(), epoch: self.epoch }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::TokioRuntime;
    use crate::{Cancellation, Clock, Spawner};

    #[tokio::test]
    async fn cancellation_resolves_after_cancel() {
        let rt = TokioRuntime::new();
        let rt2 = rt.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            rt2.cancelled().await;
            let _ = tx.send(());
        });

        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "should not be cancelled yet");

        rt.cancel();
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok(), "should be cancelled");
    }

    #[tokio::test]
    async fn is_cancelled_reflects_state() {
        let rt = TokioRuntime::new();
        assert!(!rt.is_cancelled());
        rt.cancel();
        assert!(rt.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancels_independently_of_parent() {
        let parent = TokioRuntime::new();
        let child = parent.child();

        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled(), "child cancel must not affect parent");
    }

    #[tokio::test]
    async fn parent_cancel_cancels_child() {
        let parent = TokioRuntime::new();
        let child = parent.child();

        parent.cancel();
        assert!(parent.is_cancelled());
        assert!(child.is_cancelled(), "child must be cancelled when parent is");
    }

    #[tokio::test]
    async fn spawned_task_returns_output() {
        let rt = TokioRuntime::new();
        let handle = rt.spawn(async { 42u32 });
        assert_eq!(handle.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn now_advances_with_wall_clock() {
        let rt = TokioRuntime::new();
        let t0 = rt.now();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let t1 = rt.now();
        assert!(t1 > t0, "now() must advance over real time");
    }

    #[tokio::test]
    async fn with_token_bridges_existing_token() {
        let token = CancellationToken::new();
        let rt = TokioRuntime::with_token(token.clone());

        token.cancel();
        assert!(rt.is_cancelled(), "runtime must reflect external token state");
    }

    #[tokio::test]
    async fn cancelled_is_cancel_safe_in_select() {
        let rt = TokioRuntime::new();
        let rt2 = rt.clone();

        // Run a select loop that drops the cancelled() future on each iteration.
        // After calling cancel(), the very next poll must resolve.
        for _ in 0..3 {
            tokio::select! {
                biased;
                _ = rt2.cancelled() => { panic!("must not cancel yet"); }
                _ = tokio::task::yield_now() => {}
            }
        }

        rt.cancel();

        let mut took_cancel = false;
        tokio::select! {
            biased;
            _ = rt2.cancelled() => { took_cancel = true; }
            _ = std::future::pending::<()>() => {}
        }
        assert!(took_cancel, "cancelled() must fire immediately after cancel()");
    }
}
