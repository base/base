//! Time abstraction: current virtual time, sleeping, and interval ticking.

use std::{future::Future, pin::Pin, time::Duration};

use futures::stream::BoxStream;

/// A monotonic time source with sleep and interval primitives.
///
/// `TokioRuntime` wraps `tokio::time` directly. The deterministic executor
/// (`Runner`/`Context`) only advances virtual time when all tasks are idle,
/// so timers never fire spontaneously in tests.
///
/// # Replacing `tokio::time::Interval`
///
/// [`interval`](Clock::interval) returns a `BoxStream<'static, ()>`. At call
/// sites, replace `self.interval.tick().await` with
/// `use futures::StreamExt; self.interval.next().await`.
pub trait Clock: Send + Sync + 'static {
    /// Returns elapsed virtual time since this runtime was created.
    ///
    /// For `TokioRuntime` this is wall-clock elapsed time. For the
    /// deterministic executor, it accumulates only when the executor skips
    /// idle time to the next pending alarm.
    fn now(&self) -> Duration;

    /// Returns a future that resolves after `duration` of virtual time has passed.
    ///
    /// The returned future is `'static`: it does not borrow `self`.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Returns a stream that yields `()` every `period` of virtual time.
    ///
    /// The first tick fires immediately, matching `tokio::time::interval`
    /// default behaviour. Subsequent ticks only fire after each `period`
    /// elapses (or after the deterministic executor skips idle time to cover
    /// that period in tests).
    fn interval(&self, period: Duration) -> BoxStream<'static, ()>;

    /// Returns the current wall-clock time as seconds since the Unix epoch.
    ///
    /// Used when converting on-chain Unix timestamps to the monotonic
    /// domain (e.g. recovering `resolved_at` after a restart).
    ///
    /// The default implementation calls [`std::time::SystemTime::now`].
    /// The deterministic test runtime overrides this to return a value
    /// derived from virtual time, making time-dependent code fully
    /// testable.
    fn wall_clock_unix_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs()
    }
}
