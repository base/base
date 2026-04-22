use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(30);

/// A rolling 30-second window for computing instantaneous TPS, GPS, and latency percentiles.
#[derive(Debug)]
pub struct RollingWindow {
    gas_events: VecDeque<(Instant, u64)>,
    latency_events: VecDeque<(Instant, Duration)>,
}

impl RollingWindow {
    /// Creates a new rolling window.
    pub const fn new() -> Self {
        Self { gas_events: VecDeque::new(), latency_events: VecDeque::new() }
    }

    /// Records a confirmed transaction with its gas used and latency.
    pub fn push(&mut self, gas_used: u64, latency: Duration, at: Instant) {
        self.gas_events.push_back((at, gas_used));
        self.latency_events.push_back((at, latency));
        self.prune();
    }

    /// Records a gas-only event (no latency tracking).
    pub fn push_gas(&mut self, gas_used: u64, at: Instant) {
        self.gas_events.push_back((at, gas_used));
        self.prune();
    }

    /// Records a latency-only event (no gas tracking).
    pub fn push_latency(&mut self, latency: Duration, at: Instant) {
        self.latency_events.push_back((at, latency));
        self.prune();
    }

    /// Returns the transactions-per-second rate over the rolling window.
    pub fn tps(&mut self) -> f64 {
        self.prune();
        let count = self.gas_events.len();
        if count == 0 {
            return 0.0;
        }
        let window = self.elapsed_window_secs();
        if window <= 0.0 { 0.0 } else { count as f64 / window }
    }

    /// Returns the gas-per-second rate over the rolling window.
    pub fn gps(&mut self) -> f64 {
        self.prune();
        if self.gas_events.is_empty() {
            return 0.0;
        }
        let total_gas: u64 = self.gas_events.iter().map(|(_, g)| *g).sum();
        let window = self.elapsed_window_secs();
        if window <= 0.0 { 0.0 } else { total_gas as f64 / window }
    }

    /// Returns the (p50, p99) latency percentiles over the rolling window.
    pub fn p50_p99(&mut self) -> (Duration, Duration) {
        self.prune();
        if self.latency_events.is_empty() {
            return (Duration::ZERO, Duration::ZERO);
        }
        let mut latencies: Vec<Duration> = self.latency_events.iter().map(|(_, l)| *l).collect();
        latencies.sort_unstable();
        let len = latencies.len();
        let p50 = latencies[(len * 50).div_ceil(100).saturating_sub(1).min(len - 1)];
        let p99 = latencies[(len * 99).div_ceil(100).saturating_sub(1).min(len - 1)];
        (p50, p99)
    }

    fn prune(&mut self) {
        let cutoff = Instant::now().checked_sub(WINDOW).unwrap_or_else(Instant::now);
        while self.gas_events.front().is_some_and(|(t, _)| *t < cutoff) {
            self.gas_events.pop_front();
        }
        while self.latency_events.front().is_some_and(|(t, _)| *t < cutoff) {
            self.latency_events.pop_front();
        }
    }

    /// Actual elapsed time covered by the oldest event in the window (clamped to 30s).
    fn elapsed_window_secs(&self) -> f64 {
        match self.gas_events.front() {
            Some((oldest, _)) => oldest.elapsed().as_secs_f64().min(WINDOW.as_secs_f64()),
            None => 0.0,
        }
    }
}

impl Default for RollingWindow {
    fn default() -> Self {
        Self::new()
    }
}
