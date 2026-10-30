use std::time::SystemTime;

use crate::Clock;

/// The operating system's wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    #[expect(clippy::disallowed_methods, reason = "SystemClock is the wall-clock source")]
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{Clock, SystemClock};

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "compare SystemClock with the operating system clock"
    )]
    fn system_time_when_using_default_clock() {
        let clock: SystemClock = Default::default();
        let before = SystemTime::now();

        let now = clock.now();

        assert!((before..=SystemTime::now()).contains(&now));
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "compare SystemClock with the operating system clock"
    )]
    fn unix_millis_when_using_system_clock() {
        let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();

        let millis = SystemClock.now_unix_millis().unwrap();

        let after = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();
        assert!((before..=after).contains(&millis));
    }
}
