//! Task spawning abstraction replacing `tokio::spawn`.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use thiserror::Error;

/// Errors from a spawned task.
#[derive(Debug, Error)]
pub enum TaskError {
    /// The task panicked.
    #[error("task panicked")]
    Panicked,
    /// The task was cancelled when the runtime shut down.
    #[error("task cancelled")]
    Cancelled,
}

/// A handle to a background task that can be awaited for its output.
///
/// Analogous to `tokio::task::JoinHandle<T>`. Dropping the handle does not
/// cancel the underlying task (fire-and-forget semantics).
pub struct TaskHandle<T> {
    inner: Pin<Box<dyn Future<Output = Result<T, TaskError>> + Send>>,
}

impl<T> TaskHandle<T> {
    /// Construct a [`TaskHandle`] from any future that produces `Result<T, TaskError>`.
    ///
    /// This is the only way to create a `TaskHandle`. Spawner implementations
    /// use this to wrap the underlying runtime handle.
    pub fn new(fut: impl Future<Output = Result<T, TaskError>> + Send + 'static) -> Self {
        Self { inner: Box::pin(fut) }
    }
}

impl<T> fmt::Debug for TaskHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskHandle").finish_non_exhaustive()
    }
}

impl<T> Future for TaskHandle<T> {
    type Output = Result<T, TaskError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

/// Spawn async tasks onto the runtime.
///
/// Implementations must be [`Clone`] so that spawner handles can be passed
/// into spawned tasks to allow further spawning.
pub trait Spawner: Clone + Send + Sync + 'static {
    /// Spawn a `Send + 'static` future as a background task.
    ///
    /// Returns a [`TaskHandle`] that resolves to the task's output when it
    /// completes. Dropping the handle does not cancel the task.
    fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;
}
