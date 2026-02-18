/// A cell that holds a value and allows waiting for it to be set.
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::Future;
use parking_lot::Mutex;
use tokio::sync::Notify;

/// A cell that holds a value and allows waiting for it to be set.
///
/// Values can be overwritten by calling [`BlockCell::set`] multiple times.
#[derive(Clone, Debug)]
pub struct BlockCell<T> {
    /// The inner value.
    pub inner: Arc<Mutex<Option<T>>>,
    /// Notification channel for value changes.
    pub notify: Arc<Notify>,
}

impl<T: Clone> BlockCell<T> {
    /// Creates a new empty [`BlockCell`].
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(None)), notify: Arc::new(Notify::new()) }
    }

    /// Sets the value and notifies waiters.
    pub fn set(&self, value: T) {
        let mut inner = self.inner.lock();
        *inner = Some(value);
        self.notify.notify_one();
    }

    /// Returns the current value, if set.
    pub fn get(&self) -> Option<T> {
        let inner = self.inner.lock();
        inner.clone()
    }

    /// Return a future that resolves when a value is set.
    pub fn wait_for_value(&self) -> WaitForValue<T> {
        WaitForValue { cell: self.clone() }
    }
}

/// Future that resolves when a value is set in [`BlockCell`].
#[derive(Clone)]
pub struct WaitForValue<T> {
    /// The cell being waited on.
    pub cell: BlockCell<T>,
}

impl<T> std::fmt::Debug for WaitForValue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitForValue").finish_non_exhaustive()
    }
}

impl<T: Clone> Future for WaitForValue<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.cell.get().map_or_else(
            || {
                cx.waker().wake_by_ref();
                Poll::Pending
            },
            Poll::Ready,
        )
    }
}

impl<T: Clone> Default for BlockCell<T> {
    fn default() -> Self {
        Self::new()
    }
}
