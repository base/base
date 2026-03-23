use std::time::{Duration, Instant};

use alloy_primitives::TxHash;
use tracing::debug;

use super::{MetricsAggregator, MetricsSummary, TransactionMetrics};

/// Collects transaction metrics during test execution.
#[derive(Debug)]
pub struct MetricsCollector {
    start_time: Option<Instant>,
    transactions: Vec<TransactionMetrics>,
    submitted_count: u64,
    failed_count: u64,
}

impl MetricsCollector {
    /// Creates a new metrics collector.
    pub const fn new() -> Self {
        Self { start_time: None, transactions: Vec::new(), submitted_count: 0, failed_count: 0 }
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
        debug!(tx_hash = %metrics.tx_hash, latency_ms = metrics.latency.as_millis(), "tx confirmed");
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
