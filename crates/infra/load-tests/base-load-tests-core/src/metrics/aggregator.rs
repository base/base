use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{GasMetrics, LatencyMetrics, ThroughputMetrics, TransactionMetrics};

/// Aggregates raw transaction metrics into summary statistics.
#[derive(Debug)]
pub struct MetricsAggregator<'a> {
    transactions: &'a [TransactionMetrics],
}

impl<'a> MetricsAggregator<'a> {
    /// Creates a new aggregator from transaction metrics.
    pub const fn new(transactions: &'a [TransactionMetrics]) -> Self {
        Self { transactions }
    }

    /// Computes summary statistics from the collected metrics.
    pub fn summarize(&self, duration: Duration, submitted: u64, failed: u64) -> MetricsSummary {
        MetricsSummary {
            latency: self.compute_latency(),
            throughput: self.compute_throughput(duration, submitted, failed),
            gas: self.compute_gas(),
        }
    }

    fn compute_latency(&self) -> LatencyMetrics {
        if self.transactions.is_empty() {
            return LatencyMetrics::default();
        }

        let mut latencies: Vec<Duration> = self.transactions.iter().map(|t| t.latency).collect();
        latencies.sort();

        let len = latencies.len();
        let sum: Duration = latencies.iter().sum();

        let mean = Duration::from_nanos(sum.as_nanos() as u64 / len as u64);
        LatencyMetrics {
            min: latencies[0],
            max: latencies[len - 1],
            mean,
            p50: Self::percentile(&latencies, 50),
            p95: Self::percentile(&latencies, 95),
            p99: Self::percentile(&latencies, 99),
        }
    }

    fn compute_throughput(
        &self,
        duration: Duration,
        submitted: u64,
        failed: u64,
    ) -> ThroughputMetrics {
        let confirmed = self.transactions.len() as u64;
        let tps = if duration.as_secs_f64() > 0.0 {
            confirmed as f64 / duration.as_secs_f64()
        } else {
            0.0
        };

        ThroughputMetrics {
            total_submitted: submitted,
            total_confirmed: confirmed,
            total_failed: failed,
            tps,
            duration,
        }
    }

    fn compute_gas(&self) -> GasMetrics {
        if self.transactions.is_empty() {
            return GasMetrics::default();
        }

        let total_gas: u64 = self.transactions.iter().map(|t| t.gas_used).sum();
        let total_cost: u128 = self.transactions.iter().map(|t| t.cost_wei()).sum();
        let total_gas_price: u128 = self.transactions.iter().map(|t| t.gas_price).sum();
        let count = self.transactions.len() as u64;

        GasMetrics {
            total_gas,
            avg_gas: total_gas / count,
            total_cost_wei: total_cost,
            avg_gas_price: total_gas_price / count as u128,
        }
    }

    fn percentile(sorted: &[Duration], pct: usize) -> Duration {
        let idx = (sorted.len() * pct / 100).saturating_sub(1).max(0);
        sorted[idx]
    }
}

/// Summary of all collected metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSummary {
    /// Latency statistics.
    pub latency: LatencyMetrics,
    /// Throughput statistics.
    pub throughput: ThroughputMetrics,
    /// Gas usage statistics.
    pub gas: GasMetrics,
}

impl MetricsSummary {
    /// Serializes the summary to JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}
