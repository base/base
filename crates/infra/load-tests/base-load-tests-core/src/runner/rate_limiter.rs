use std::time::{Duration, Instant};

use tokio::time::sleep;

/// Rate limiter for controlling gas throughput.
#[derive(Debug)]
pub struct RateLimiter {
    target_gps: u64,
    avg_gas_per_tx: u64,
    interval: Duration,
    last_tick: Option<Instant>,
}

impl RateLimiter {
    /// Creates a new rate limiter for the target gas per second.
    pub fn new(target_gps: u64, avg_gas_per_tx: u64) -> Self {
        let tps = if avg_gas_per_tx > 0 { target_gps / avg_gas_per_tx } else { 100 };
        let interval = if tps == 0 {
            Duration::from_secs(1)
        } else {
            Duration::from_secs_f64(1.0 / tps as f64)
        };
        Self { target_gps, avg_gas_per_tx, interval, last_tick: None }
    }

    /// Updates the average gas per transaction and recalculates the interval.
    pub fn update_avg_gas(&mut self, avg_gas: u64) {
        if avg_gas > 0 && avg_gas != self.avg_gas_per_tx {
            self.avg_gas_per_tx = avg_gas;
            let tps = self.target_gps / avg_gas;
            self.interval = if tps == 0 {
                Duration::from_secs(1)
            } else {
                Duration::from_secs_f64(1.0 / tps as f64)
            };
        }
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

    /// Returns the current effective TPS based on target GPS and avg gas.
    pub const fn effective_tps(&self) -> u64 {
        if self.avg_gas_per_tx > 0 { self.target_gps / self.avg_gas_per_tx } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_interval() {
        let limiter = RateLimiter::new(210_000, 21_000);
        assert_eq!(limiter.effective_tps(), 10);
        assert_eq!(limiter.interval(), Duration::from_millis(100));

        let limiter = RateLimiter::new(2_100_000, 21_000);
        assert_eq!(limiter.effective_tps(), 100);
        assert_eq!(limiter.interval(), Duration::from_millis(10));
    }
}
