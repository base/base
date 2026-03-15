//! Time abstraction: current virtual time, sleeping, and interval ticking.

use std::{future::Future, pin::Pin, time::Duration};

use futures::stream::BoxStream;

/// A monotonic time source with sleep and interval primitives.
///
/// [`TokioRuntime`] wraps `tokio::time` directly. [`DeterministicRuntime`]
/// only advances when [`DeterministicRuntime::advance_time`] is called, so
/// timers never fire spontaneously in tests.
///
/// # Replacing `tokio::time::Interval`
///
/// [`interval`](Clock::interval) returns a `BoxStream<'static, ()>`. At call
/// sites, replace `self.interval.tick().await` with
/// `use futures::StreamExt; self.interval.next().await`.
///
/// [`TokioRuntime`]: crate::TokioRuntime
/// [`DeterministicRuntime`]: crate::DeterministicRuntime
pub trait Clock: Send + Sync + 'static {
    /// Returns elapsed virtual time since this runtime was created.
    ///
    /// For [`TokioRuntime`] this is wall-clock elapsed time. For
    /// [`DeterministicRuntime`] it accumulates only via
    /// [`advance_time`](crate::DeterministicRuntime::advance_time).
    ///
    /// [`TokioRuntime`]: crate::TokioRuntime
    fn now(&self) -> Duration;

    /// Returns a future that resolves after `duration` of virtual time has passed.
    ///
    /// The returned future is `'static`: it does not borrow `self`.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// Returns a stream that yields `()` every `period` of virtual time.
    ///
    /// The first tick fires immediately, matching `tokio::time::interval`
    /// default behaviour. Subsequent ticks only fire after each `period`
    /// elapses (or after [`advance_time`](crate::DeterministicRuntime::advance_time)
    /// covers that period in tests).
    fn interval(&self, period: Duration) -> BoxStream<'static, ()>;
}
