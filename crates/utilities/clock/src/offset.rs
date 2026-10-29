use std::time::{Instant, SystemTime};

use crate::Clock;

/// A wall clock anchored to a chosen epoch, advancing at real speed.
#[derive(Debug, Clone)]
pub struct OffsetClock {
    start: SystemTime,
    anchor: Instant,
}

impl OffsetClock {
    /// Starts a clock at `start`, without pausing or changing its rate.
    pub fn new(start: SystemTime) -> Self {
        Self { start, anchor: Instant::now() }
    }
}

impl Clock for OffsetClock {
    fn now(&self) -> SystemTime {
        self.start + self.anchor.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::{Duration, Instant, SystemTime},
    };

    use crate::{Clock, OffsetClock};

    #[test]
    fn starts_at_requested_epoch_when_created() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let before = Instant::now();

        let now = OffsetClock::new(start).now();

        assert!((start..=start + before.elapsed()).contains(&now));
    }

    #[test]
    fn advances_at_real_speed_when_time_elapses() {
        let start = SystemTime::UNIX_EPOCH;
        let before_creation = Instant::now();
        let clock = OffsetClock::new(start);
        let after_creation = Instant::now();
        let initial = clock.now();
        thread::spawn(|| {}).join().unwrap();
        let lower_bound = start + after_creation.elapsed();

        let now = clock.now();

        let upper_bound = start + before_creation.elapsed();
        assert!((lower_bound..=upper_bound).contains(&now));
        assert!(now > initial);
    }

    #[test]
    fn unix_millis_error_when_before_epoch() {
        let clock = OffsetClock::new(SystemTime::UNIX_EPOCH - Duration::from_secs(86_400));

        let result = clock.now_unix_millis();

        assert!(result.is_err());
    }
}
