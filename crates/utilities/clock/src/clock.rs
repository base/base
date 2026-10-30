use std::{
    fmt::Debug,
    time::{SystemTime, SystemTimeError},
};

/// A thread-safe wall-clock time source.
pub trait Clock: Send + Sync + Debug {
    /// Returns the current wall-clock time.
    fn now(&self) -> SystemTime;

    /// Returns whole milliseconds since the Unix epoch.
    ///
    /// # Errors
    /// Returns [`SystemTimeError`] if the time precedes the Unix epoch.
    fn now_unix_millis(&self) -> Result<u128, SystemTimeError> {
        Ok(self.now().duration_since(SystemTime::UNIX_EPOCH)?.as_millis())
    }
}
