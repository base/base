use std::{collections::HashMap, time::Duration};

use alloy_primitives::TxHash;
use tracing::debug;

use super::{MetricsAggregator, MetricsSummary, RollingWindow, TransactionMetrics};

/// Collects transaction metrics during test execution.
#[derive(Debug)]
pub struct MetricsCollector {
    transactions: Vec<TransactionMetrics>,
    submitted_count: u64,
    failed_count: u64,
    failure_reasons: HashMap<String, u64>,
    rolling: RollingWindow,
    flashblocks_rolling: RollingWindow,
}

impl MetricsCollector {
    /// Creates a new metrics collector.
    pub fn new() -> Self {
        Self {
            transactions: Vec::new(),
            submitted_count: 0,
            failed_count: 0,
            failure_reasons: HashMap::new(),
            rolling: RollingWindow::new(),
            flashblocks_rolling: RollingWindow::new(),
        }
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
        } else {
            self.rolling.push_gas(metrics.gas_used);
        }
        if let Some(flashblocks_latency) = metrics.flashblocks_latency {
            self.flashblocks_rolling.push_latency(flashblocks_latency);
        }
        self.transactions.push(metrics);
    }

    /// Records a failed transaction with a categorized reason.
    pub fn record_failed(&mut self, _tx_hash: TxHash, reason: &str) {
        self.failed_count += 1;
        *self.failure_reasons.entry(reason.to_string()).or_insert(0) += 1;
    }

    /// Records multiple failures with the same reason (e.g. expired txs
    /// reported in bulk after the confirmer shuts down).
    pub fn record_failures(&mut self, reason: &str, count: u64) {
        self.failed_count += count;
        *self.failure_reasons.entry(reason.to_string()).or_insert(0) += count;
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
    ///
    /// `active_duration` should cover only the active submission window
    /// (first tx submitted → last tx submitted), excluding setup and
    /// confirmation-drain phases.
    pub fn summarize(&self, active_duration: Duration) -> MetricsSummary {
        let aggregator = MetricsAggregator::new(&self.transactions);
        aggregator.summarize(
            active_duration,
            self.submitted_count,
            self.failed_count,
            &self.failure_reasons,
        )
    }

    /// Resets the collector for reuse.
    pub fn reset(&mut self) {
        self.transactions.clear();
        self.submitted_count = 0;
        self.failed_count = 0;
        self.failure_reasons.clear();
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
