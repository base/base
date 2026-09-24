//! Runtime gate for retiring Flashblocks RPC behavior at the Denim activation timestamp.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Shared, irreversible runtime gate for the Denim RPC cutover.
///
/// The predicate is evaluated on every RPC entry until it first returns `true`. At that point the
/// gate is latched permanently, ensuring a runtime upgrade-signal update cannot accidentally
/// reactivate Flashblocks behavior after clients have observed the post-Denim API.
#[derive(Clone)]
pub struct FlashblocksRpcCutover {
    active: Arc<AtomicBool>,
    activation_predicate: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl fmt::Debug for FlashblocksRpcCutover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksRpcCutover")
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl FlashblocksRpcCutover {
    /// Creates a cutover gate backed by an activation predicate.
    pub fn new(activation_predicate: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            activation_predicate: Arc::new(activation_predicate),
        }
    }

    /// Returns whether the node must use post-Denim RPC behavior.
    pub fn is_active(&self) -> bool {
        if self.active.load(Ordering::Acquire) {
            return true;
        }

        if !(self.activation_predicate)() {
            return false;
        }

        self.active.store(true, Ordering::Release);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;

    #[test]
    fn activates_at_the_boundary_and_stays_active() {
        let now = Arc::new(AtomicU64::new(99));
        let clock = Arc::clone(&now);
        let cutover = FlashblocksRpcCutover::new(move || clock.load(Ordering::Relaxed) >= 100);

        assert!(!cutover.is_active());

        now.store(100, Ordering::Relaxed);
        assert!(cutover.is_active());

        now.store(0, Ordering::Relaxed);
        assert!(cutover.is_active());
    }
}
