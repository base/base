use std::time::Duration;

/// Adaptive backoff tracker that adjusts based on consecutive errors.
#[derive(Debug, Clone)]
pub struct AdaptiveBackoff {
    base_ms: u64,
    max_ms: u64,
    current_ms: u64,
    consecutive_errors: u32,
    consecutive_successes: u32,
    error_threshold: u32,
    success_threshold: u32,
}

impl AdaptiveBackoff {
    /// Creates a new adaptive backoff with the given base and max delays.
    pub const fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms,
            max_ms,
            current_ms: base_ms,
            consecutive_errors: 0,
            consecutive_successes: 0,
            error_threshold: 3,
            success_threshold: 10,
        }
    }

    /// Records a successful operation, potentially decreasing backoff.
    pub fn record_success(&mut self) {
        self.consecutive_errors = 0;
        self.consecutive_successes += 1;

        if self.consecutive_successes >= self.success_threshold {
            self.current_ms = (self.current_ms / 2).max(self.base_ms);
            self.consecutive_successes = 0;
        }
    }

    /// Records a failed operation, potentially increasing backoff.
    pub fn record_error(&mut self) {
        self.consecutive_successes = 0;
        self.consecutive_errors += 1;

        if self.consecutive_errors >= self.error_threshold {
            self.current_ms = (self.current_ms * 2).min(self.max_ms);
        }
    }

    /// Returns the current backoff duration.
    pub const fn current(&self) -> Duration {
        Duration::from_millis(self.current_ms)
    }

    /// Returns the number of consecutive errors.
    pub const fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors
    }

    /// Resets the backoff to its initial state.
    pub const fn reset(&mut self) {
        self.current_ms = self.base_ms;
        self.consecutive_errors = 0;
        self.consecutive_successes = 0;
    }
}

impl Default for AdaptiveBackoff {
    fn default() -> Self {
        Self::new(50, 5000)
    }
}
