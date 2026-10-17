//! Process-wide candidate state shared by ingress, the pool and the builder.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{CandidateAccessPolicy, CandidateLimits, GroupLedger};

/// Per-process runtime kill switch (design §8.4).
///
/// Off: ingress returns `candidates_disabled`, and pooled groups follow lifecycle step 4. A
/// single process switch is not a cluster switch; operators must flip every potential leader.
#[derive(Debug)]
pub struct CandidateSwitch {
    enabled: AtomicBool,
}

impl CandidateSwitch {
    /// Creates a switch in the given state.
    pub const fn new(enabled: bool) -> Self {
        Self { enabled: AtomicBool::new(enabled) }
    }

    /// Returns whether candidate admission and selection are enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Sets the switch and returns the previous state.
    pub fn set(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::AcqRel)
    }
}

impl Default for CandidateSwitch {
    fn default() -> Self {
        Self::new(true)
    }
}

/// Shared candidate state: the ledger, the access policy, the kill switch and ingress limits.
#[derive(Debug)]
pub struct CandidateRuntime {
    /// Group ledger.
    pub ledger: GroupLedger,
    /// Access policy.
    pub policy: Arc<dyn CandidateAccessPolicy>,
    /// Runtime kill switch.
    pub switch: CandidateSwitch,
    /// Ingress bounds.
    pub limits: CandidateLimits,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_reports_previous_state() {
        let switch = CandidateSwitch::new(false);
        assert!(!switch.is_enabled());
        assert!(!switch.set(true));
        assert!(switch.is_enabled());
        assert!(switch.set(false));
    }
}
