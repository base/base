//! Timestamp planning for sequencer block production.

use std::time::Duration;

const MILLIS_PER_SECOND: u64 = 1_000;
/// Fixed sub-second cadence used after Zombie activation.
pub(super) const ZOMBIE_BLOCK_INTERVAL: Duration = Duration::from_millis(200);

/// A planned block timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerTimestamp {
    /// Whole-second timestamp.
    pub timestamp: u64,
    /// Sub-second millisecond remainder, present once Zombie is active. When set, this is
    /// always a multiple of the active cadence (e.g. one of 0, 200, 400, 600, 800 for the
    /// current 200ms `ZOMBIE_BLOCK_INTERVAL`).
    pub timestamp_millis_part: Option<u16>,
    /// Millisecond timestamp, i.e. `timestamp * 1_000 + timestamp_millis_part.unwrap_or(0)`.
    pub timestamp_millis: u64,
}

/// Errors produced while planning a block timestamp.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SequencerTimestampPlannerError {
    /// Timestamp arithmetic overflowed `u64`.
    #[error("sequencer timestamp overflowed u64")]
    TimestampOverflow,
}

/// Plans the next block timestamp for the sequencer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SequencerTimestampPlanner;

impl SequencerTimestampPlanner {
    /// Plans a legacy timestamp by advancing the parent by `block_time` seconds.
    pub fn legacy(
        parent_timestamp: u64,
        block_time: u64,
    ) -> Result<SequencerTimestamp, SequencerTimestampPlannerError> {
        let timestamp = parent_timestamp
            .checked_add(block_time)
            .ok_or(SequencerTimestampPlannerError::TimestampOverflow)?;
        let timestamp_millis = timestamp
            .checked_mul(MILLIS_PER_SECOND)
            .ok_or(SequencerTimestampPlannerError::TimestampOverflow)?;
        Ok(SequencerTimestamp { timestamp, timestamp_millis_part: None, timestamp_millis })
    }

    /// Plans a sub-second timestamp aligned to `cadence_millis`.
    ///
    /// Floors wall-clock to the cadence grid; a monotonicity guard ensures the
    /// result is at least one cadence step past the parent.
    pub fn sub_second(
        parent_timestamp_millis: u64,
        wall_clock_millis: u64,
        cadence_millis: u64,
    ) -> Result<SequencerTimestamp, SequencerTimestampPlannerError> {
        let candidate = (wall_clock_millis / cadence_millis) * cadence_millis;
        let minimum = parent_timestamp_millis
            .checked_add(cadence_millis)
            .ok_or(SequencerTimestampPlannerError::TimestampOverflow)?;
        let timestamp_millis = candidate.max(minimum);
        let timestamp = timestamp_millis / MILLIS_PER_SECOND;
        let timestamp_millis_part = (timestamp_millis % MILLIS_PER_SECOND) as u16;
        Ok(SequencerTimestamp {
            timestamp,
            timestamp_millis_part: Some(timestamp_millis_part),
            timestamp_millis,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_advances_by_block_time() {
        let planned = SequencerTimestampPlanner::legacy(1_000, 2).expect("legacy must succeed");
        assert_eq!(planned.timestamp, 1_002);
        assert_eq!(planned.timestamp_millis_part, None);
        assert_eq!(planned.timestamp_millis, 1_002_000);
    }

    #[test]
    fn sub_second_floors_wall_clock_to_cadence() {
        let planned = SequencerTimestampPlanner::sub_second(1_000_000, 1_000_450, 200)
            .expect("sub-second planning must succeed");
        assert_eq!(planned.timestamp_millis, 1_000_400);
        assert_eq!(planned.timestamp, 1_000);
        assert_eq!(planned.timestamp_millis_part, Some(400));
    }

    #[test]
    fn sub_second_enforces_monotonic_minimum() {
        let planned = SequencerTimestampPlanner::sub_second(1_000_000, 1_000_050, 200)
            .expect("sub-second planning must succeed");
        assert_eq!(planned.timestamp_millis, 1_000_200);
        assert_eq!(planned.timestamp_millis_part, Some(200));
    }

    #[test]
    fn sub_second_carries_across_second_boundary() {
        let planned = SequencerTimestampPlanner::sub_second(1_999_800, 2_000_050, 200)
            .expect("sub-second planning must succeed");
        assert_eq!(planned.timestamp_millis, 2_000_000);
        assert_eq!(planned.timestamp, 2_000);
        assert_eq!(planned.timestamp_millis_part, Some(0));
    }

    #[test]
    fn sub_second_overflow_is_rejected() {
        assert_eq!(
            SequencerTimestampPlanner::sub_second(u64::MAX, 0, 200),
            Err(SequencerTimestampPlannerError::TimestampOverflow)
        );
    }
}
