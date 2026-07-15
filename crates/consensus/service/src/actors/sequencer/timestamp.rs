//! Timestamp planning for sequencer block production.
//!
//! Prior to the Zombie upgrade blocks advance by the legacy `block_time` in
//! whole seconds. Once Zombie is active blocks advance on a 200ms cadence, and
//! the resulting timestamp is split into a whole-second component and a
//! millisecond remainder (`timestamp_millis_part`).

use super::config::BASE_BLOCK_TIME_MILLIS;

const ZOMBIE_CADENCE_MILLIS: u64 = BASE_BLOCK_TIME_MILLIS;

/// The number of milliseconds in one second.
const MILLIS_PER_SECOND: u64 = 1_000;

/// A planned block timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerTimestamp {
    /// The whole-second component of the timestamp (seconds since the Unix epoch).
    pub timestamp: u64,
    /// The millisecond component (0-999, multiple of 200), or [`None`] pre-Zombie.
    pub timestamp_millis_part: Option<u16>,
    /// The full timestamp in milliseconds since the Unix epoch.
    pub timestamp_millis: u64,
}

/// Errors produced while planning a block timestamp.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SequencerTimestampPlannerError {
    /// The computed timestamp overflowed a [`u64`].
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

    /// Plans a Zombie timestamp aligned to the 200ms cadence.
    ///
    /// The candidate is the wall clock floored to the cadence; a monotonicity
    /// guard ensures the result is at least one cadence step past the parent.
    pub fn zombie(
        parent_timestamp_millis: u64,
        wall_clock_millis: u64,
    ) -> Result<SequencerTimestamp, SequencerTimestampPlannerError> {
        let candidate = (wall_clock_millis / ZOMBIE_CADENCE_MILLIS) * ZOMBIE_CADENCE_MILLIS;
        let minimum = parent_timestamp_millis
            .checked_add(ZOMBIE_CADENCE_MILLIS)
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
        let planned = SequencerTimestampPlanner::legacy(1_000, 2).unwrap();
        assert_eq!(planned.timestamp, 1_002);
        assert_eq!(planned.timestamp_millis_part, None);
        assert_eq!(planned.timestamp_millis, 1_002_000);
    }

    #[test]
    fn zombie_floors_wall_clock_to_cadence() {
        let planned = SequencerTimestampPlanner::zombie(1_000_000, 1_000_450).unwrap();
        assert_eq!(planned.timestamp_millis, 1_000_400);
        assert_eq!(planned.timestamp, 1_000);
        assert_eq!(planned.timestamp_millis_part, Some(400));
    }

    #[test]
    fn zombie_enforces_monotonic_minimum() {
        let planned = SequencerTimestampPlanner::zombie(1_000_000, 1_000_050).unwrap();
        assert_eq!(planned.timestamp_millis, 1_000_200);
        assert_eq!(planned.timestamp_millis_part, Some(200));
    }

    #[test]
    fn zombie_carries_across_second_boundary() {
        let planned = SequencerTimestampPlanner::zombie(1_999_800, 2_000_050).unwrap();
        assert_eq!(planned.timestamp_millis, 2_000_000);
        assert_eq!(planned.timestamp, 2_000);
        assert_eq!(planned.timestamp_millis_part, Some(0));
    }

    #[test]
    fn zombie_overflow_is_rejected() {
        assert_eq!(
            SequencerTimestampPlanner::zombie(u64::MAX, 0),
            Err(SequencerTimestampPlannerError::TimestampOverflow)
        );
    }
}
