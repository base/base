use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    sync::OnceLock,
};

use crate::{Clock, SystemClock};

static GLOBAL_CLOCK: OnceLock<Box<dyn Clock>> = OnceLock::new();

/// Access to the process-wide wall clock.
#[derive(Debug)]
pub struct GlobalClock;

impl GlobalClock {
    /// Returns the installed clock, or [`SystemClock`] before installation.
    ///
    /// Reading the default does not prevent subsequent installation. Fetch the
    /// clock on each use: a previously returned reference remains unchanged.
    pub fn get() -> &'static dyn Clock {
        match GLOBAL_CLOCK.get() {
            Some(clock) => clock.as_ref(),
            None => &SystemClock,
        }
    }

    /// Installs a clock for the remaining lifetime of the process.
    ///
    /// Install before starting worker threads that need the chosen epoch.
    ///
    /// # Errors
    /// Returns [`SetClockError`] if a clock was already installed.
    pub fn set(clock: impl Clock + 'static) -> Result<(), SetClockError> {
        GLOBAL_CLOCK.set(Box::new(clock)).map_err(|_| SetClockError)
    }
}

/// A process-wide clock has already been installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetClockError;

impl Display for SetClockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("a global clock has already been installed")
    }
}

impl Error for SetClockError {}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::{Duration, Instant, SystemTime},
    };

    use crate::{GlobalClock, OffsetClock, SetClockError, SystemClock};

    // Keep this lifecycle in one test: the process-wide clock cannot be reset.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "verify the default clock against real wall time"
    )]
    fn defaults_to_system_time_then_preserves_first_installed_clock() {
        let before = SystemTime::now();
        let default_now = GlobalClock::get().now();
        assert!((before..=SystemTime::now()).contains(&default_now));

        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let before_installation = Instant::now();
        GlobalClock::set(OffsetClock::new(start)).unwrap();
        let installed_now = thread::spawn(|| GlobalClock::get().now()).join().unwrap();
        assert!((start..=start + before_installation.elapsed()).contains(&installed_now));

        assert_eq!(GlobalClock::set(SystemClock), Err(SetClockError));
        let retained_now = GlobalClock::get().now();
        assert!((installed_now..=start + before_installation.elapsed()).contains(&retained_now));
    }
}
