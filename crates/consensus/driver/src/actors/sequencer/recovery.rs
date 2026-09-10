//! Shared recovery mode flag for the sequencer.
//!
//! [`RecoveryModeGuard`] is a cheaply-cloneable wrapper around an atomic bool
//! so the sequencer actor and [`super::build::PayloadBuilder`] always observe
//! the same value without passing it as a parameter on every call.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Shared flag indicating whether the sequencer is in recovery mode.
///
/// All clones share the same underlying [`AtomicBool`]. The actor writes via
/// [`RecoveryModeGuard::set`] (e.g. from the admin API) and the builder reads
/// via [`RecoveryModeGuard::get`] on each build attempt.
#[derive(Debug, Clone)]
pub struct RecoveryModeGuard {
    inner: Arc<AtomicBool>,
}

impl RecoveryModeGuard {
    /// Creates a new guard with the given initial state.
    pub fn new(initial: bool) -> Self {
        Self { inner: Arc::new(AtomicBool::new(initial)) }
    }

    /// Returns the current recovery mode state.
    pub fn get(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }

    /// Sets the recovery mode state.
    pub fn set(&self, value: bool) {
        self.inner.store(value, Ordering::Release);
    }
}
