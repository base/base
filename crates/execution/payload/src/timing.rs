//! Wall-clock timing for one-shot Denim sequencer builds.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base_protocol::BaseTimeUpdateTx;

/// Absolute wall-clock deadline (unix milliseconds) for pulling pool
/// transactions into a Denim-active block.
///
/// The slot for a block with millisecond timestamp `T_N` starts at
/// `T_{N-1} = T_N - BLOCK_INTERVAL_MILLIS`; the CL calls `engine_getPayload`
/// at `slot_start + seal_offset`, so the build stops pulling pool transactions
/// at that same instant and proceeds to the normal finish (state root,
/// freeze). Wall-clock anchoring makes catch-up automatic: a build started
/// late gets a shorter window (or none), never a shifted schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxCutoff(u64);

impl TxCutoff {
    /// Computes the cutoff for the block with the given millisecond timestamp:
    /// `block_timestamp_ms - BLOCK_INTERVAL_MILLIS + seal_offset`.
    pub fn new(block_timestamp_ms: u64, seal_offset: Duration) -> Self {
        let slot_start =
            block_timestamp_ms.saturating_sub(u64::from(BaseTimeUpdateTx::BLOCK_INTERVAL_MILLIS));
        let offset = u64::try_from(seal_offset.as_millis()).unwrap_or(u64::MAX);
        Self(slot_start.saturating_add(offset))
    }

    /// Returns `true` once the wall clock has reached the cutoff.
    pub fn is_past(&self) -> bool {
        Self::unix_now_millis() >= self.0
    }

    /// The cutoff as unix milliseconds.
    pub const fn unix_millis(&self) -> u64 {
        self.0
    }

    fn unix_now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_is_slot_start_plus_seal_offset() {
        // Block timestamp 1_800_000_001_200ms => slot starts at ..._000ms.
        let cutoff = TxCutoff::new(1_800_000_001_200, Duration::from_millis(150));
        assert_eq!(cutoff.unix_millis(), 1_800_000_001_150);
    }

    #[test]
    fn past_cutoff_is_past() {
        assert!(TxCutoff::new(0, Duration::ZERO).is_past());
    }

    #[test]
    fn future_cutoff_is_not_past() {
        // A block timestamp far in the future yields a cutoff that has not passed.
        let year_10k_ms = 253_402_300_800_000;
        assert!(!TxCutoff::new(year_10k_ms, Duration::from_millis(150)).is_past());
    }
}
