//! `OnceLock` abstraction that uses [`std::sync::OnceLock`] when available, once_cell otherwise.

#![allow(dead_code)]

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
#[cfg(feature = "std")]
pub(crate) use std::sync::OnceLock;

#[cfg(feature = "std")]
use once_cell as _;
#[cfg(not(feature = "std"))]
use once_cell::race::OnceBox;

/// A thread-safe cell which can be written to only once.
#[cfg(not(feature = "std"))]
#[derive(Debug)]
pub(crate) struct OnceLock<T> {
    inner: OnceBox<T>,
}

#[cfg(not(feature = "std"))]
impl<T> Default for OnceLock<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(feature = "std"))]
impl<T> OnceLock<T> {
    /// Creates a new empty OnceLock.
    #[inline]
    pub(crate) const fn new() -> Self {
        Self { inner: OnceBox::new() }
    }

    /// Gets the contents of the OnceLock, initializing it if necessary.
    #[inline]
    pub(crate) fn get_or_init<F>(&self, f: F) -> &T
    where
        F: FnOnce() -> T,
    {
        self.inner.get_or_init(|| Box::new(f()))
    }

    /// Gets the contents of the OnceLock, returning None if it is not initialized.
    #[inline]
    pub(crate) fn get(&self) -> Option<&T> {
        self.inner.get()
    }

    /// Sets the contents of the OnceLock.
    #[inline]
    pub(crate) fn set(&self, value: T) -> Result<(), T> {
        self.inner.set(Box::new(value)).map_err(|value| *value)
    }
}
