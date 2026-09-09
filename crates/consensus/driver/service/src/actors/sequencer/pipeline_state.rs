//! Loop-scoped state tracking the build/seal pipeline across ticks of the sequencer main loop.

use std::time::{Duration, Instant};

use base_protocol::L2BlockInfo;

use crate::UnsealedPayloadHandle;

/// Build/seal pipeline bookkeeping carried between iterations of the sequencer main loop.
///
/// [`Default`] gives the empty state the actor starts with: no pre-built payload, no queued
/// parent, and no prior seal/completion timing.
#[derive(Debug, Default)]
pub struct BuildPipelineState {
    /// Pre-built payload awaiting sealing.
    pub next_payload_to_seal: Option<UnsealedPayloadHandle>,
    /// Acknowledged parent whose child build is gated on the parent's timestamp.
    pub pending_build_parent: Option<L2BlockInfo>,
    /// Duration of the most recently completed seal, used as the ticker's lead time for
    /// pre-Cobalt blocks only. Cobalt-active blocks seal at a fixed offset into their slot
    /// and ignore this value.
    pub last_seal_duration: Duration,
    /// Wall-clock instant the most recent block finished, used to record
    /// [`crate::Metrics::sequencer_block_to_block_duration`].
    pub last_block_complete_at: Option<Instant>,
}

impl BuildPipelineState {
    /// Records that a block has just completed, returning the elapsed time since the previous
    /// completion. Returns `None` on the first block after construction or after a stop/start
    /// cycle cleared the previous timestamp, so that idle time is never recorded as block time.
    pub fn record_block_complete(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let elapsed = self.last_block_complete_at.map(|prev| now.duration_since(prev));
        self.last_block_complete_at = Some(now);
        elapsed
    }

    /// Clears state that must not carry across a stop -> start transition: any queued parent
    /// build and the previous completion timestamp (so the first block after restart does not
    /// record the idle period as block-to-block duration).
    pub const fn clear_for_active_transition(&mut self) {
        self.pending_build_parent = None;
        self.last_block_complete_at = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::BuildPipelineState;

    #[test]
    fn record_block_complete_returns_none_first_time() {
        let mut state = BuildPipelineState::default();
        assert!(state.record_block_complete().is_none());
    }

    #[test]
    fn record_block_complete_returns_elapsed_on_subsequent_calls() {
        let mut state = BuildPipelineState::default();
        state.record_block_complete();
        std::thread::sleep(Duration::from_millis(5));
        assert!(state.record_block_complete().is_some());
    }

    #[test]
    fn clear_for_active_transition_resets_fields() {
        let mut state = BuildPipelineState::default();
        state.record_block_complete();
        state.clear_for_active_transition();
        assert!(state.last_block_complete_at.is_none());
        assert!(state.pending_build_parent.is_none());
    }
}
