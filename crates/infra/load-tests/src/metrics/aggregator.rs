use std::{collections::HashMap, time::Duration};

use serde::{Deserialize, Serialize};

use super::{
    FlashblocksLatencyMetrics, GasMetrics, LatencyMetrics, ThroughputMetrics,
    ThroughputPercentiles, TransactionMetrics,
};

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
    pub fn summarize(
        &self,
        duration: Duration,
        submitted: u64,
        failed: u64,
        failure_reasons: &HashMap<String, u64>,
        tps_samples: &[f64],
        gps_samples: &[f64],
    ) -> MetricsSummary {
        let mut top_failure_reasons: Vec<(String, u64)> =
            failure_reasons.iter().map(|(k, v)| (k.clone(), *v)).collect();
        top_failure_reasons.sort_by(|a, b| b.1.cmp(&a.1));
        top_failure_reasons.truncate(3);

        MetricsSummary {
            block_latency: self.compute_block_latency(),
            flashblocks_latency: self.compute_flashblocks_latency(),
            throughput: self.compute_throughput(duration, submitted, failed),
            throughput_percentiles: Self::compute_throughput_percentiles(tps_samples, gps_samples),
            gas: self.compute_gas(),
            top_failure_reasons,
        }
    }

    fn compute_block_latency(&self) -> LatencyMetrics {
        let mut latencies: Vec<Duration> =
            self.transactions.iter().filter_map(|t| t.block_latency).collect();

        if latencies.is_empty() {
            return LatencyMetrics::default();
        }

        latencies.sort();

        let len = latencies.len();
        let sum: Duration = latencies.iter().sum();
        let mean = Duration::from_nanos((sum.as_nanos() / len as u128) as u64);

        LatencyMetrics {
            min: latencies[0],
            max: latencies[len - 1],
            mean,
            p50: Self::percentile(&latencies, 50),
            p95: Self::percentile(&latencies, 95),
            p99: Self::percentile(&latencies, 99),
        }
    }

    fn compute_flashblocks_latency(&self) -> FlashblocksLatencyMetrics {
        let mut latencies: Vec<Duration> =
            self.transactions.iter().filter_map(|t| t.flashblocks_latency).collect();

        if latencies.is_empty() {
            return FlashblocksLatencyMetrics::default();
        }

        latencies.sort();

        let len = latencies.len();
        let sum: Duration = latencies.iter().sum();
        let mean = Duration::from_nanos((sum.as_nanos() / len as u128) as u64);

        FlashblocksLatencyMetrics {
            count: len as u64,
            min: latencies[0],
            max: latencies[len - 1],
            mean,
            p50: Self::percentile(&latencies, 50),
            p90: Self::percentile(&latencies, 90),
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
        let total_gas: u64 = self.transactions.iter().map(|t| t.gas_used).sum();
        let duration_secs = duration.as_secs_f64();

        let (tps, gps) = if duration_secs > 0.0 {
            (confirmed as f64 / duration_secs, total_gas as f64 / duration_secs)
        } else {
            (0.0, 0.0)
        };

        ThroughputMetrics {
            total_submitted: submitted,
            total_confirmed: confirmed,
            total_failed: failed,
            tps,
            gps,
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

    fn compute_throughput_percentiles(
        tps_samples: &[f64],
        gps_samples: &[f64],
    ) -> ThroughputPercentiles {
        if tps_samples.is_empty() {
            return ThroughputPercentiles::default();
        }

        let mut tps = tps_samples.to_vec();
        let mut gps = gps_samples.to_vec();
        tps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        gps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        ThroughputPercentiles {
            tps_p50: Self::f64_percentile(&tps, 50),
            tps_p90: Self::f64_percentile(&tps, 90),
            tps_p99: Self::f64_percentile(&tps, 99),
            tps_max: tps.last().copied().unwrap_or(0.0),
            gps_p50: Self::f64_percentile(&gps, 50),
            gps_p90: Self::f64_percentile(&gps, 90),
            gps_p99: Self::f64_percentile(&gps, 99),
            gps_max: gps.last().copied().unwrap_or(0.0),
        }
    }

    fn percentile(sorted: &[Duration], pct: usize) -> Duration {
        let rank = (sorted.len() * pct).div_ceil(100);
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        sorted[idx]
    }

    fn f64_percentile(sorted: &[f64], pct: usize) -> f64 {
        let rank = (sorted.len() * pct).div_ceil(100);
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        sorted[idx]
    }
}

/// Summary of all collected metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSummary {
    /// Block production latency.
    pub block_latency: LatencyMetrics,
    /// Flashblocks sequencer latency.
    pub flashblocks_latency: FlashblocksLatencyMetrics,
    /// Throughput statistics.
    pub throughput: ThroughputMetrics,
    /// Rolling-window throughput percentiles (TPS and GPS).
    pub throughput_percentiles: ThroughputPercentiles,
    /// Gas usage statistics.
    pub gas: GasMetrics,
    /// Top failure reasons sorted by count descending (max 3).
    pub top_failure_reasons: Vec<(String, u64)>,
}

impl MetricsSummary {
    /// Serializes the summary to JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}
