//! Deterministic runtime for reproducible async testing.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{StreamExt, stream::BoxStream};
use tokio::sync::watch;
use tokio_stream::wrappers::IntervalStream;

use crate::{Cancellation, Clock, Spawner, TaskError, TaskHandle};

/// Deterministic runtime for reproducible integration and unit tests.
///
/// The clock only advances when [`advance_time`](DeterministicRuntime::advance_time)
/// is called — timers never fire from wall-clock time. All other async
/// operations (task spawning, cancellation) use the tokio runtime that backs
/// the test.
///
/// # Requirements
///
/// - Must be created and used inside a `#[tokio::test]` context.
/// - [`advance_time`](DeterministicRuntime::advance_time) requires the tokio
///   time driver to be paused. Use `#[tokio::test(start_paused = true)]` in
///   any test that calls `advance_time`.
///
/// # Example
///
/// ```rust,ignore
/// #[tokio::test(start_paused = true)]
/// async fn test_batcher_channel_timeout() {
///     let rt = DeterministicRuntime::new();
///     let source = HybridBlockSource::new(rt.clone(), subscription, poller, Duration::from_secs(1));
///     let driver = BatchDriver::new(rt.clone(), pipeline, source, tx_manager, ...);
///
///     tokio::spawn(driver.run());
///
///     // Advance 2 seconds of virtual time — fires the polling interval twice.
///     rt.advance_time(Duration::from_secs(2)).await;
///
///     // Assert expected state, then cancel.
///     rt.cancel();
/// }
/// ```
#[derive(Clone, Debug)]
pub struct DeterministicRuntime {
    /// Accumulated virtual time, advanced by `advance_time`.
    virtual_now: Arc<Mutex<Duration>>,
    /// Cancellation channel: `true` means cancelled.
    cancel_tx: Arc<watch::Sender<bool>>,
}

impl DeterministicRuntime {
    /// Create a new deterministic runtime with virtual time starting at zero.
    ///
    /// Must be called inside a `#[tokio::test]` context.
    pub fn new() -> Self {
        // Unused receiver is dropped immediately; the Sender retains the channel.
        let (cancel_tx, _) = watch::channel(false);
        Self { virtual_now: Arc::new(Mutex::new(Duration::ZERO)), cancel_tx: Arc::new(cancel_tx) }
    }

    /// Advance virtual time by `duration` and poll all tasks that become ready.
    ///
    /// This calls `tokio::time::advance(duration)` which wakes every
    /// `tokio::time::sleep` and `tokio::time::interval` future whose deadline
    /// falls within the advanced window. A final `yield_now` lets woken tasks
    /// run to their next suspension point before this method returns.
    ///
    /// # Panics
    ///
    /// Panics if the tokio time driver is not paused. Use
    /// `#[tokio::test(start_paused = true)]` to ensure time is paused.
    pub async fn advance_time(&self, duration: Duration) {
        *self.virtual_now.lock().expect("virtual_now lock poisoned") += duration;
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
    }
}

impl Default for DeterministicRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for DeterministicRuntime {
    fn now(&self) -> Duration {
        *self.virtual_now.lock().expect("virtual_now lock poisoned")
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // tokio::time::sleep respects the paused clock and only fires when
        // advance_time advances past the deadline.
        Box::pin(tokio::time::sleep(duration))
    }

    fn interval(&self, period: Duration) -> BoxStream<'static, ()> {
        // tokio::time::interval also respects the paused clock.
        IntervalStream::new(tokio::time::interval(period)).map(|_| ()).boxed()
    }
}

impl Spawner for DeterministicRuntime {
    fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = tokio::spawn(future);
        TaskHandle {
            inner: Box::pin(async move {
                handle.await.map_err(|e| {
                    if e.is_cancelled() { TaskError::Cancelled } else { TaskError::Panicked }
                })
            }),
        }
    }
}

impl Cancellation for DeterministicRuntime {
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // Subscribe creates a receiver initialised with the *current* value of
        // the channel. If cancel() was already called (value = true), the very
        // first borrow() in the returned future returns true immediately, making
        // this cancel-safe: dropping and recreating the future never misses a
        // cancellation that happened while it was not being polled.
        let mut rx = self.cancel_tx.subscribe();
        Box::pin(async move {
            // Fast path: already cancelled.
            if *rx.borrow() {
                return;
            }
            // Slow path: wait for the value to change. changed() is cancel-safe:
            // if this future is dropped and recreated, the subscribe() above will
            // start the new receiver at the current (potentially true) value.
            loop {
                match rx.changed().await {
                    Ok(()) => {
                        if *rx.borrow() {
                            return;
                        }
                    }
                    // Sender dropped: treat as cancellation.
                    Err(_) => return,
                }
            }
        })
    }

    fn cancel(&self) {
        // send_replace unconditionally updates the stored value even when no
        // receivers are subscribed, unlike send() which returns Err and leaves
        // the value unchanged when all receivers have been dropped.
        self.cancel_tx.send_replace(true);
    }

    fn is_cancelled(&self) -> bool {
        *self.cancel_tx.borrow()
    }

    fn child(&self) -> Self {
        // Phase 1: children share the same cancellation scope.
        // A full parent/child DAG (child can cancel independently) can be
        // added later if needed.
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;

    use super::DeterministicRuntime;
    use crate::{Cancellation, Clock, Spawner};

    #[tokio::test(start_paused = true)]
    async fn sleep_does_not_fire_without_advance() {
        let rt = DeterministicRuntime::new();
        let rt2 = rt.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            rt2.sleep(Duration::from_secs(5)).await;
            let _ = tx.send(());
        });

        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "sleep must not fire before advance");

        rt.advance_time(Duration::from_secs(5)).await;
        assert!(rx.try_recv().is_ok(), "sleep must fire after advance");
    }

    #[tokio::test(start_paused = true)]
    async fn advance_time_updates_now() {
        let rt = DeterministicRuntime::new();
        assert_eq!(rt.now(), Duration::ZERO);

        rt.advance_time(Duration::from_secs(3)).await;
        assert_eq!(rt.now(), Duration::from_secs(3));

        rt.advance_time(Duration::from_millis(500)).await;
        assert_eq!(rt.now(), Duration::from_millis(3_500));
    }

    #[tokio::test(start_paused = true)]
    async fn interval_does_not_tick_without_advance() {
        let rt = DeterministicRuntime::new();
        let mut stream = rt.interval(Duration::from_secs(1));

        // First tick fires immediately (tokio::time::interval semantics).
        assert!(stream.next().await.is_some(), "first tick must fire immediately");

        // Second tick must not fire without advancing.
        let rt2 = rt.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            stream.next().await;
            let _ = tx.send(());
        });

        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "second tick must not fire before advance");

        rt2.advance_time(Duration::from_secs(1)).await;
        assert!(rx.try_recv().is_ok(), "second tick must fire after 1s advance");
    }

    #[tokio::test]
    async fn cancellation_resolves_after_cancel() {
        let rt = DeterministicRuntime::new();
        let rt2 = rt.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            rt2.cancelled().await;
            let _ = tx.send(());
        });

        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "must not be cancelled yet");

        rt.cancel();
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok(), "must be cancelled after cancel()");
    }

    #[tokio::test]
    async fn is_cancelled_reflects_state() {
        let rt = DeterministicRuntime::new();
        assert!(!rt.is_cancelled());
        rt.cancel();
        assert!(rt.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_resolves_immediately_if_already_cancelled() {
        let rt = DeterministicRuntime::new();
        rt.cancel();

        // cancelled() on an already-cancelled runtime must resolve without
        // needing any advance or yield.
        tokio::time::timeout(Duration::from_millis(10), rt.cancelled())
            .await
            .expect("cancelled() must resolve immediately when already cancelled");
    }

    #[tokio::test]
    async fn cancelled_is_cancel_safe_in_select() {
        let rt = DeterministicRuntime::new();
        let rt2 = rt.clone();

        // Drop cancelled() repeatedly without it firing; then cancel and
        // verify that the very next poll fires despite prior drops.
        for _ in 0..5 {
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
        assert!(took_cancel, "cancelled() must fire on next poll after cancel()");
    }

    #[tokio::test]
    async fn spawned_task_returns_output() {
        let rt = DeterministicRuntime::new();
        let handle = rt.spawn(async { 42u32 });
        assert_eq!(handle.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn spawned_task_captures_closure() {
        let rt = DeterministicRuntime::new();
        let value = 99u64;
        let handle = rt.spawn(async move { value * 2 });
        assert_eq!(handle.await.unwrap(), 198);
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_advances_accumulate() {
        let rt = DeterministicRuntime::new();
        let rt2 = rt.clone();
        let rt3 = rt.clone();
        let (tx1, mut rx1) = tokio::sync::oneshot::channel::<()>();
        let (tx2, mut rx2) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            rt2.sleep(Duration::from_secs(2)).await;
            let _ = tx1.send(());
        });
        tokio::spawn(async move {
            rt3.sleep(Duration::from_secs(5)).await;
            let _ = tx2.send(());
        });

        tokio::task::yield_now().await;
        assert!(rx1.try_recv().is_err());
        assert!(rx2.try_recv().is_err());

        rt.advance_time(Duration::from_secs(2)).await;
        assert!(rx1.try_recv().is_ok(), "2s sleep must fire after 2s advance");
        assert!(rx2.try_recv().is_err(), "5s sleep must not fire after 2s advance");

        rt.advance_time(Duration::from_secs(3)).await;
        assert!(rx2.try_recv().is_ok(), "5s sleep must fire after total 5s advance");
    }
}
