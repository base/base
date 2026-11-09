//! Runtime switch for process-global transaction event emission.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Opens and closes process-global transaction event emission at runtime.
///
/// A producer whose events only matter in one role, such as a sequencer builder that is the
/// active leader, closes the gate while it is not in that role. While the gate is closed,
/// [`GlobalTransactionEventWriter::get`](crate::GlobalTransactionEventWriter::get) returns
/// `None`, so call sites skip building the event entirely instead of journaling and dropping it.
///
/// Clones share one switch. A new gate is open.
#[derive(Clone, Debug)]
pub struct TransactionEventGate {
    open: Arc<AtomicBool>,
}

impl Default for TransactionEventGate {
    fn default() -> Self {
        Self::new(true)
    }
}

impl TransactionEventGate {
    /// Creates a gate in the given state.
    pub fn new(open: bool) -> Self {
        Self { open: Arc::new(AtomicBool::new(open)) }
    }

    /// Returns whether events currently pass through the gate.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// Opens or closes the gate for every clone.
    pub fn set_open(&self, open: bool) {
        self.open.store(open, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_switch() {
        let gate = TransactionEventGate::default();
        let clone = gate.clone();
        assert!(gate.is_open());

        clone.set_open(false);
        assert!(!gate.is_open());

        gate.set_open(true);
        assert!(clone.is_open());
    }
}
