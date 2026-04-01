use std::time::{Duration, Instant};

use alloy_primitives::TxHash;
use tracing::debug;

use super::{MetricsAggregator, MetricsSummary, RollingWindow, TransactionMetrics};

/// Collects transaction metrics during test execution.
#[derive(Debug)]
pub struct MetricsCollector {
    start_time: Option<Instant>,
    transactions: Vec<TransactionMetrics>,
    submitted_count: u64,
    failed_count: u64,
    rolling: RollingWindow,
    flashblocks_rolling: RollingWindow,
}

impl MetricsCollector {
    /// Creates a new metrics collector.
    pub const fn new() -> Self {
        Self {
            start_time: None,
            transactions: Vec::new(),
            submitted_count: 0,
            failed_count: 0,
            rolling: RollingWindow::new(),
            flashblocks_rolling: RollingWindow::new(),
        }
    }

    /// Starts the metrics collection timer.
    pub fn start(&mut self) {
        self.start_time = Some(Instant::now());
        debug!("metrics collection started");
    }

    /// Records a submitted transaction.
    pub const fn record_submitted(&mut self, _tx_hash: TxHash) {
        self.submitted_count += 1;
    }

    /// Records a confirmed transaction with metrics.
    pub fn record_confirmed(&mut self, metrics: TransactionMetrics) {
        debug!(tx_hash = %metrics.tx_hash, block_latency_ms = ?metrics.block_latency.map(|d| d.as_millis()), "tx confirmed");
        if let Some(latency) = metrics.block_latency {
            self.rolling.push(metrics.gas_used, latency);
        }
        if let Some(flashblocks_latency) = metrics.flashblocks_latency {
            self.flashblocks_rolling.push_latency(flashblocks_latency);
        }
        self.transactions.push(metrics);
    }

    /// Records a failed transaction.
    pub const fn record_failed(&mut self, _tx_hash: TxHash, _reason: &str) {
        self.failed_count += 1;
    }

    /// Returns the elapsed time since start.
    pub fn elapsed(&self) -> Duration {
        self.start_time.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Returns the number of confirmed transactions.
    pub const fn confirmed_count(&self) -> usize {
        self.transactions.len()
    }

    /// Returns the number of submitted transactions.
    pub const fn submitted_count(&self) -> u64 {
        self.submitted_count
    }

    /// Returns the number of failed transactions.
    pub const fn failed_count(&self) -> u64 {
        self.failed_count
    }

    /// Generates a summary of collected metrics.
    pub fn summarize(&self) -> MetricsSummary {
        let aggregator = MetricsAggregator::new(&self.transactions);
        aggregator.summarize(self.elapsed(), self.submitted_count, self.failed_count)
    }

    /// Resets the collector for reuse.
    pub fn reset(&mut self) {
        self.start_time = None;
        self.transactions.clear();
        self.submitted_count = 0;
        self.failed_count = 0;
        self.rolling = RollingWindow::new();
        self.flashblocks_rolling = RollingWindow::new();
    }

    /// Returns the rolling 30s TPS.
    pub fn rolling_tps(&mut self) -> f64 {
        self.rolling.tps()
    }

    /// Returns the rolling 30s GPS.
    pub fn rolling_gps(&mut self) -> f64 {
        self.rolling.gps()
    }

    /// Returns the rolling 30s (p50, p99) latency percentiles.
    pub fn rolling_p50_p99(&mut self) -> (std::time::Duration, std::time::Duration) {
        self.rolling.p50_p99()
    }

    /// Rolling 30s flashblocks (p50, p99).
    pub fn rolling_flashblocks_p50_p99(&mut self) -> (std::time::Duration, std::time::Duration) {
        self.flashblocks_rolling.p50_p99()
    }

    /// Returns the average gas used per confirmed transaction.
    pub fn avg_gas_used(&self) -> Option<u64> {
        if self.transactions.is_empty() {
            return None;
        }
        let total: u64 = self.transactions.iter().map(|t| t.gas_used).sum();
        Some(total / self.transactions.len() as u64)
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}
