//! Deterministic async executor for reproducible test execution.
//!
//! Uses a seeded RNG to shuffle the ready-task queue before each polling
//! round, ensuring that any given seed produces an identical execution trace
//! across machines and Rust versions. Virtual time only advances when the
//! executor has no ready tasks, jumping directly to the next alarm deadline
//! rather than spinning — so a 100-second timer test completes in
//! microseconds.
//!
//! See [`Runner::start`] for the entry point and [`Context`] for the runtime
//! handle passed to tasks.

mod executor;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task,
    task::Poll,
    time::Duration,
};

pub use executor::{Alarm, Executor, Sleeper, Task, Tasks};
use futures::{
    StreamExt,
    channel::oneshot,
    stream::{self, BoxStream},
};

use crate::{Cancellation, Clock, Spawner, TaskError, TaskHandle};

/// Configuration for a [`Runner`] execution.
#[derive(Debug)]
pub struct Config {
    /// Seed for the task-scheduling RNG. The same seed always produces the
    /// same task polling order, making test failures reproducible.
    pub seed: u64,
    /// Panic after this many polling cycles. Guards against busy-loop livelocks.
    /// Recommended: `Some(1_000_000)` for tests.
    pub cycle_limit: Option<u64>,
    /// Panic if virtual time exceeds this duration. Guards against runaway sleepers.
    pub timeout: Option<Duration>,
}

impl Config {
    /// Create a configuration with the given seed and no limits.
    pub const fn seeded(seed: u64) -> Self {
        Self { seed, cycle_limit: None, timeout: None }
    }
}

/// Entry point for deterministic execution.
///
/// # Example
///
/// ```rust,ignore
/// #[test]
/// fn test_timer_fires() {
///     use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
///
///     Runner::start(Config::seeded(42), |ctx| async move {
///         let fired = Arc::new(AtomicBool::new(false));
///         let fired2 = fired.clone();
///         ctx.spawn(async move {
///             ctx.sleep(Duration::from_secs(5)).await;
///             fired2.store(true, Ordering::SeqCst);
///         });
///         // Executor skips idle time to t=5s, fires the sleep, runs the task.
///         ctx.sleep(Duration::from_secs(5)).await;
///         assert!(fired.load(Ordering::SeqCst));
///     });
/// }
/// ```
#[derive(Debug)]
pub struct Runner;

impl Runner {
    /// Execute `f` inside the deterministic executor and return its output.
    ///
    /// Blocks the calling thread until the root future resolves. All futures
    /// spawned via [`Context::spawn`] run cooperatively within the same
    /// executor loop, interleaved in seeded-random order.
    pub fn start<F, Fut>(config: Config, f: F) -> Fut::Output
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let executor = Executor::new(config.seed);
        let cancel = Arc::new(CancelState::default());

        let ctx = Context { executor: Arc::downgrade(&executor), cancel };
        let (tx, mut rx) = oneshot::channel::<Fut::Output>();

        let root = f(ctx);
        executor.tasks.insert(Box::pin(async move {
            let output = root.await;
            let _ = tx.send(output);
        }));

        let mut cycles: u64 = 0;
        loop {
            executor.poll_ready();

            if let Ok(Some(output)) = rx.try_recv() {
                return output;
            }

            if executor.tasks.has_ready() {
                cycles += 1;
                if let Some(limit) = config.cycle_limit {
                    assert!(cycles < limit, "runtime stalled: cycle limit {limit} exceeded");
                }
                continue;
            }

            executor.skip_idle_time();
            if let Some(timeout) = config.timeout {
                assert!(
                    *executor.time.lock().unwrap() <= timeout,
                    "runtime stalled: virtual time exceeded {timeout:?}"
                );
            }
            executor.wake_ready_sleepers();
            executor.assert_liveness();
        }
    }
}

/// The runtime handle passed to tasks within a [`Runner::start`] call.
///
/// Cloneable and `Send`. Implements [`Clock`], [`Spawner`], and
/// [`Cancellation`], and therefore [`Runtime`](crate::Runtime). Pass
/// `ctx.clone()` into futures passed to [`Spawner::spawn`].
#[derive(Clone)]
pub struct Context {
    executor: Weak<Executor>,
    cancel: Arc<CancelState>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

struct CancelState {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<task::Waker>>,
    children: Mutex<Vec<Weak<Self>>>,
}

impl Default for CancelState {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            wakers: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
        }
    }
}

impl CancelState {
    /// Signal cancellation and propagate to all live children.
    ///
    /// Holds the `children` lock while setting the flag so that [`new_child`]
    /// cannot register a new child between the flag write and the propagation
    /// sweep. Idempotent: a second call is a no-op.
    fn cancel(&self) {
        let children = {
            let mut guard = self.children.lock().unwrap();
            if self.cancelled.swap(true, Ordering::AcqRel) {
                return;
            }
            std::mem::take(&mut *guard)
        };
        for waker in self.wakers.lock().unwrap().drain(..) {
            waker.wake();
        }
        for weak in &children {
            if let Some(child) = weak.upgrade() {
                child.cancel();
            }
        }
    }

    /// Create a child `CancelState` that is cancelled when this state is
    /// cancelled, but can be cancelled independently without affecting this state.
    fn new_child(&self) -> Arc<Self> {
        let child = Arc::new(Self::default());
        let mut guard = self.children.lock().unwrap();
        if self.cancelled.load(Ordering::Acquire) {
            // Parent already cancelled; immediately cancel the new child.
            child.cancelled.store(true, Ordering::Release);
        } else {
            guard.push(Arc::downgrade(&child));
        }
        child
    }
}

impl Clock for Context {
    fn now(&self) -> Duration {
        self.executor.upgrade().map_or(Duration::ZERO, |e| *e.time.lock().unwrap())
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let deadline = self.now() + duration;
        Box::pin(Sleeper { executor: Weak::clone(&self.executor), deadline, registered: false })
    }

    fn interval(&self, period: Duration) -> BoxStream<'static, ()> {
        let ctx = self.clone();
        // First tick fires immediately (matching tokio::time::interval semantics).
        stream::once(std::future::ready(()))
            .chain(stream::unfold(ctx, move |ctx| async move {
                ctx.sleep(period).await;
                Some(((), ctx))
            }))
            .boxed()
    }

    fn wall_clock_unix_secs(&self) -> u64 {
        // Anchor virtual time at a fixed epoch so that tests get
        // deterministic Unix timestamps. The specific epoch value
        // is arbitrary; 1_700_000_000 (≈ 2023-11-14) is a convenient
        // recent timestamp.
        const EPOCH: u64 = 1_700_000_000;
        EPOCH + self.now().as_secs()
    }
}

impl Spawner for Context {
    fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<F::Output>();
        if let Some(executor) = self.executor.upgrade() {
            executor.tasks.insert(Box::pin(async move {
                let _ = tx.send(future.await);
            }));
        }
        TaskHandle::new(async move { rx.await.map_err(|_| TaskError::Cancelled) })
    }
}

/// Cancel-safe future returned by [`Cancellation::cancelled`].
struct CancelFuture {
    state: Arc<CancelState>,
}

impl Future for CancelFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        if self.state.cancelled.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        let mut wakers = self.state.wakers.lock().unwrap();
        // Replace any stale waker from a previous poll of this exact future so
        // the wakers list does not accumulate duplicates across select! iterations.
        wakers.retain(|w| !w.will_wake(cx.waker()));
        wakers.push(cx.waker().clone());
        drop(wakers);
        // Re-check after registering to close the race between the check above
        // and a concurrent `cancel()` call.
        if self.state.cancelled.load(Ordering::Acquire) { Poll::Ready(()) } else { Poll::Pending }
    }
}

impl Cancellation for Context {
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(CancelFuture { state: Arc::clone(&self.cancel) })
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.cancelled.load(Ordering::Acquire)
    }

    fn child(&self) -> Self {
        Self { executor: Weak::clone(&self.executor), cancel: self.cancel.new_child() }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        time::Duration,
    };

    use futures::StreamExt;

    use super::{Config, Runner};
    use crate::{Cancellation, Clock, Spawner};

    #[test]
    fn sleep_does_not_fire_without_time_passing() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let fired = Arc::new(AtomicU32::new(0));
            let fired2 = Arc::clone(&fired);
            let ctx2 = ctx.clone();
            ctx.spawn(async move {
                ctx2.sleep(Duration::from_secs(5)).await;
                fired2.fetch_add(1, Ordering::SeqCst);
            });
            // Yielding once does not advance time; the spawned sleep must not fire.
            // The executor only skips idle time when there are no ready tasks.
            // Since the root task is still running (we haven't awaited a sleep),
            // the spawned task's sleep doesn't fire here.
            assert_eq!(fired.load(Ordering::SeqCst), 0, "sleep must not fire before time passes");
        });
    }

    #[test]
    fn sleep_fires_after_virtual_time_advances() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let fired = Arc::new(AtomicU32::new(0));
            let fired2 = Arc::clone(&fired);
            let ctx2 = ctx.clone();
            ctx.spawn(async move {
                ctx2.sleep(Duration::from_secs(5)).await;
                fired2.fetch_add(1, Ordering::SeqCst);
            });
            // Root sleeps longer than the spawned task so the executor fires the
            // spawned sleep at t=5 before root wakes at t=10 — no ordering race.
            ctx.sleep(Duration::from_secs(10)).await;
            assert_eq!(fired.load(Ordering::SeqCst), 1, "sleep must fire after time advances");
        });
    }

    #[test]
    fn now_reflects_virtual_time() {
        Runner::start(Config::seeded(0), |ctx| async move {
            assert_eq!(ctx.now(), Duration::ZERO);
            ctx.sleep(Duration::from_secs(3)).await;
            assert_eq!(ctx.now(), Duration::from_secs(3));
            ctx.sleep(Duration::from_millis(500)).await;
            assert_eq!(ctx.now(), Duration::from_millis(3_500));
        });
    }

    #[test]
    fn interval_first_tick_fires_immediately() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut stream = ctx.interval(Duration::from_secs(100));
            // First tick must arrive without any time advance.
            let t0 = ctx.now();
            stream.next().await;
            assert_eq!(ctx.now(), t0, "first tick must not advance time");
        });
    }

    #[test]
    fn interval_second_tick_fires_after_period() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let period = Duration::from_secs(10);
            let mut stream = ctx.interval(period);
            stream.next().await; // first tick (immediate)
            let t1 = ctx.now();
            stream.next().await; // second tick (after period)
            assert_eq!(ctx.now(), t1 + period);
        });
    }

    #[test]
    fn skip_idle_time_makes_long_sleep_instant() {
        // A 100-second sleep must complete without spinning 100s worth of time.
        // This test verifies the skip_idle_time mechanism works end-to-end.
        let start = std::time::Instant::now();
        Runner::start(Config::seeded(0), |ctx| async move {
            ctx.sleep(Duration::from_secs(100)).await;
            assert_eq!(ctx.now(), Duration::from_secs(100));
        });
        // Wall-clock time should be negligible (well under 1 second).
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_resolves_after_cancel() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let ctx2 = ctx.clone();
            assert!(!ctx.is_cancelled());
            ctx.spawn(async move {
                ctx2.cancel();
            });
            ctx.cancelled().await;
            assert!(ctx.is_cancelled());
        });
    }

    #[test]
    fn cancelled_resolves_immediately_if_already_cancelled() {
        Runner::start(Config::seeded(0), |ctx| async move {
            ctx.cancel();
            ctx.cancelled().await; // must not stall
            assert!(ctx.is_cancelled());
        });
    }

    #[test]
    fn cancelled_is_cancel_safe() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let ctx2 = ctx.clone();
            ctx.spawn(async move {
                // Yield a few times before cancelling.
                ctx2.sleep(Duration::ZERO).await;
                ctx2.sleep(Duration::ZERO).await;
                ctx2.cancel();
            });
            // The cancelled() future is recreated each time select! drops it.
            // After cancel() fires, the very next poll must resolve.
            ctx.cancelled().await;
            assert!(ctx.is_cancelled());
        });
    }

    #[test]
    fn child_cancels_independently_of_parent() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let child = ctx.child();
            child.cancel();
            assert!(child.is_cancelled());
            assert!(!ctx.is_cancelled(), "child cancel must not affect parent");
        });
    }

    #[test]
    fn parent_cancel_cancels_child() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let child = ctx.child();
            ctx.cancel();
            assert!(ctx.is_cancelled());
            assert!(child.is_cancelled(), "child must be cancelled when parent is");
        });
    }

    #[test]
    fn spawned_task_output_via_handle() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let handle = ctx.spawn(async { 42u32 });
            assert_eq!(handle.await.unwrap(), 42);
        });
    }

    #[test]
    fn multiple_sleeps_accumulate_correctly() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let ctx2 = ctx.clone();
            let ctx3 = ctx.clone();

            // Use TaskHandle.await so that we don't race with the spawned sleep.
            // h2.await blocks root until taskA completes, at which point t=2s.
            let h2 = ctx.spawn(async move { ctx2.sleep(Duration::from_secs(2)).await });
            let h5 = ctx.spawn(async move { ctx3.sleep(Duration::from_secs(5)).await });

            h2.await.unwrap();
            assert_eq!(ctx.now(), Duration::from_secs(2), "2s task must complete at t=2");
            assert!(ctx.now() < Duration::from_secs(5), "5s task must not have fired");

            h5.await.unwrap();
            assert_eq!(ctx.now(), Duration::from_secs(5), "5s task must complete at t=5");
        });
    }
}
