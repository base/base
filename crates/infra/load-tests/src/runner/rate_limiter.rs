use std::time::{Duration, Instant};

use tokio::time::sleep;

/// Rate limiter for controlling gas throughput.
///
/// Paces transaction submission so total gas consumed per second stays near
/// `target_gps`. Call [`tick_batch`](Self::tick_batch) once per batch with
/// the number of transactions in the batch; the limiter will sleep just
/// long enough to stay on target.
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
        let interval = Self::compute_interval(target_gps, avg_gas_per_tx);
        Self { target_gps, avg_gas_per_tx, interval, last_tick: None }
    }

    fn compute_interval(target_gps: u64, avg_gas_per_tx: u64) -> Duration {
        if avg_gas_per_tx == 0 {
            return Duration::from_millis(10);
        }
        let tps = target_gps as f64 / avg_gas_per_tx as f64;
        if tps <= 0.0 { Duration::from_secs(1) } else { Duration::from_secs_f64(1.0 / tps) }
    }

    /// Updates the average gas per transaction and recalculates the interval.
    pub fn update_avg_gas(&mut self, avg_gas: u64) {
        if avg_gas > 0 && avg_gas != self.avg_gas_per_tx {
            self.avg_gas_per_tx = avg_gas;
            self.interval = Self::compute_interval(self.target_gps, avg_gas);
        }
    }

    /// Sleeps to pace a batch of `count` transactions against the gas target.
    ///
    /// The required delay is `count * per_tx_interval` minus any time already
    /// elapsed since the previous call. Returns immediately on first call or
    /// when the elapsed time already exceeds the budget.
    pub async fn tick_batch(&mut self, count: usize) {
        let budget = self.interval.saturating_mul(count as u32);
        match self.last_tick {
            None => {
                self.last_tick = Some(Instant::now());
            }
            Some(last) => {
                let elapsed = last.elapsed();
                if elapsed < budget {
                    sleep(budget - elapsed).await;
                }
                self.last_tick = Some(Instant::now());
            }
        }
    }

    /// Resets the tick timer to now, preventing credit accumulation during pauses.
    pub fn reset_tick(&mut self) {
        self.last_tick = Some(Instant::now());
    }

    /// Returns the interval between ticks.
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Returns the current effective TPS based on target GPS and avg gas.
    pub fn effective_tps(&self) -> f64 {
        if self.avg_gas_per_tx > 0 {
            self.target_gps as f64 / self.avg_gas_per_tx as f64
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_interval() {
        let limiter = RateLimiter::new(210_000, 21_000);
        assert!((limiter.effective_tps() - 10.0).abs() < 0.001);
        assert_eq!(limiter.interval(), Duration::from_millis(100));

        let limiter = RateLimiter::new(2_100_000, 21_000);
        assert!((limiter.effective_tps() - 100.0).abs() < 0.001);
        assert_eq!(limiter.interval(), Duration::from_millis(10));

        // Test non-exact division (previously truncated to 1 TPS, now correctly ~1.9 TPS)
        let limiter = RateLimiter::new(40_000, 21_000);
        assert!((limiter.effective_tps() - 1.905).abs() < 0.001);
        assert!(limiter.interval() < Duration::from_millis(526));
    }
}
