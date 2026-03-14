use std::time::{Duration, Instant};

use tokio::time::sleep;

/// Simple rate limiter for controlling TPS.
#[derive(Debug)]
pub struct RateLimiter {
    interval: Duration,
    last_tick: Option<Instant>,
}

impl RateLimiter {
    /// Creates a new rate limiter for the target TPS.
    pub fn new(tps: u64) -> Self {
        let interval = if tps == 0 {
            Duration::from_secs(1)
        } else {
            Duration::from_secs_f64(1.0 / tps as f64)
        };
        Self { interval, last_tick: None }
    }

    /// Waits until the next tick. Returns immediately on first call.
    pub async fn tick(&mut self) {
        match self.last_tick {
            None => {
                self.last_tick = Some(Instant::now());
            }
            Some(last) => {
                let elapsed = last.elapsed();
                if elapsed < self.interval {
                    sleep(self.interval - elapsed).await;
                }
                self.last_tick = Some(Instant::now());
            }
        }
    }

    /// Returns the interval between ticks.
    pub const fn interval(&self) -> Duration {
        self.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_interval() {
        let limiter = RateLimiter::new(10);
        assert_eq!(limiter.interval(), Duration::from_millis(100));

        let limiter = RateLimiter::new(100);
        assert_eq!(limiter.interval(), Duration::from_millis(10));
    }
}
