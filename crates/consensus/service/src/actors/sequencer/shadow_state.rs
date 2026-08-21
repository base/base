//! Loop-scoped shadow sequencing state: the build/reconciliation cycle plus its in-flight
//! reconciliation task, carried between iterations of the sequencer main loop.

use base_protocol::L2BlockInfo;

use crate::{EngineClientError, ShadowCycle, ShadowReconciliationTask};

/// Shadow sequencing state carried between iterations of the sequencer main loop while the actor
/// is running as a shadow sequencer.
#[derive(Debug)]
pub struct ShadowSequencingState {
    /// Progress through the current private build/reconciliation cycle.
    pub cycle: ShadowCycle,
    /// Background task performing one reconciliation attempt, if any.
    pub reconciliation_task: Option<ShadowReconciliationTask>,
}

impl ShadowSequencingState {
    /// Starts a fresh private build cycle on the provided canonical head.
    pub fn new(canonical_head: L2BlockInfo) -> Result<Self, EngineClientError> {
        Ok(Self { cycle: ShadowCycle::building(canonical_head)?, reconciliation_task: None })
    }

    /// Returns whether the cycle has reached its private block limit and is waiting for
    /// canonical payloads to reconcile.
    pub const fn is_awaiting_reconciliation(&self) -> bool {
        self.cycle.reconciliation_target().is_some()
    }

    /// Aborts any in-flight reconciliation task.
    pub fn abort_reconciliation(&mut self) {
        if let Some(task) = self.reconciliation_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::ShadowSequencingState;

    fn head(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                number,
                hash: B256::with_last_byte(number as u8),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn new_state_is_not_awaiting_reconciliation() {
        let state = ShadowSequencingState::new(head(0)).unwrap();
        assert!(!state.is_awaiting_reconciliation());
    }

    #[test]
    fn abort_reconciliation_is_a_no_op_without_a_task() {
        let mut state = ShadowSequencingState::new(head(0)).unwrap();
        state.abort_reconciliation();
        assert!(state.reconciliation_task.is_none());
    }
}
