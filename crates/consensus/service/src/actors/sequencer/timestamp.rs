//! Timestamp planning for sequencer-produced payload attributes.

use base_common_consensus::{BASE_BLOCK_TIME_MILLIS, TIMESTAMP_MILLIS_PER_SECOND};

/// Planned timestamp for a sequencer-produced payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequencerTimestamp {
    /// Seconds-denominated Engine API timestamp.
    pub timestamp: u64,
    /// Optional Base millisecond subsecond component.
    pub timestamp_millis_part: Option<u16>,
    /// Full internal timestamp in Unix milliseconds.
    pub timestamp_millis: u64,
}

/// Errors returned while planning sequencer timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequencerTimestampPlannerError {
    /// Timestamp seconds overflowed while converting to milliseconds.
    #[error("timestamp seconds overflowed when converted to milliseconds")]
    TimestampOverflow,
}

/// Planner for legacy seconds-based and post-Beryl millisecond sequencer timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequencerTimestampPlanner;

impl SequencerTimestampPlanner {
    /// Plans the legacy seconds-denominated child timestamp.
    pub const fn legacy_timestamp(
        parent_timestamp: u64,
        block_time: u64,
    ) -> Result<SequencerTimestamp, SequencerTimestampPlannerError> {
        let timestamp = match parent_timestamp.checked_add(block_time) {
            Some(timestamp) => timestamp,
            None => return Err(SequencerTimestampPlannerError::TimestampOverflow),
        };
        let timestamp_millis = match timestamp.checked_mul(TIMESTAMP_MILLIS_PER_SECOND as u64) {
            Some(timestamp_millis) => timestamp_millis,
            None => return Err(SequencerTimestampPlannerError::TimestampOverflow),
        };

        Ok(SequencerTimestamp { timestamp, timestamp_millis_part: None, timestamp_millis })
    }

    /// Plans a post-Beryl child timestamp from a known parent millisecond timestamp.
    pub const fn beryl_timestamp(
        parent_timestamp_millis: u64,
        wall_clock_timestamp_millis: u64,
    ) -> Result<SequencerTimestamp, SequencerTimestampPlannerError> {
        let cadence = BASE_BLOCK_TIME_MILLIS as u64;
        let candidate_timestamp_millis = wall_clock_timestamp_millis / cadence * cadence;
        let minimum_timestamp_millis = match parent_timestamp_millis.checked_add(cadence) {
            Some(timestamp_millis) => timestamp_millis,
            None => return Err(SequencerTimestampPlannerError::TimestampOverflow),
        };
        let timestamp_millis = if candidate_timestamp_millis > minimum_timestamp_millis {
            candidate_timestamp_millis
        } else {
            minimum_timestamp_millis
        };
        let timestamp = timestamp_millis / TIMESTAMP_MILLIS_PER_SECOND as u64;
        let timestamp_millis_part = (timestamp_millis % TIMESTAMP_MILLIS_PER_SECOND as u64) as u16;

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
    fn legacy_timestamp_preserves_seconds_cadence_without_millis_part() {
        let timestamp = SequencerTimestampPlanner::legacy_timestamp(100, 2).unwrap();

        assert_eq!(timestamp.timestamp, 102);
        assert_eq!(timestamp.timestamp_millis_part, None);
        assert_eq!(timestamp.timestamp_millis, 102_000);
    }

    #[test]
    fn beryl_timestamp_advances_within_same_second() {
        let timestamp = SequencerTimestampPlanner::beryl_timestamp(100_000, 100_123).unwrap();

        assert_eq!(timestamp.timestamp, 100);
        assert_eq!(timestamp.timestamp_millis_part, Some(200));
        assert_eq!(timestamp.timestamp_millis, 100_200);
    }

    #[test]
    fn beryl_timestamp_rolls_over_to_next_second() {
        let timestamp = SequencerTimestampPlanner::beryl_timestamp(100_800, 100_999).unwrap();

        assert_eq!(timestamp.timestamp, 101);
        assert_eq!(timestamp.timestamp_millis_part, Some(0));
        assert_eq!(timestamp.timestamp_millis, 101_000);
    }

    #[test]
    fn beryl_timestamp_clamps_lagging_wall_clock_to_parent_minimum() {
        let timestamp = SequencerTimestampPlanner::beryl_timestamp(100_400, 100_100).unwrap();

        assert_eq!(timestamp.timestamp, 100);
        assert_eq!(timestamp.timestamp_millis_part, Some(600));
        assert_eq!(timestamp.timestamp_millis, 100_600);
    }

    #[test]
    fn beryl_timestamp_can_skip_aligned_slots_when_wall_clock_is_ahead() {
        let timestamp = SequencerTimestampPlanner::beryl_timestamp(100_000, 101_340).unwrap();

        assert_eq!(timestamp.timestamp, 101);
        assert_eq!(timestamp.timestamp_millis_part, Some(200));
        assert_eq!(timestamp.timestamp_millis, 101_200);
    }
}
