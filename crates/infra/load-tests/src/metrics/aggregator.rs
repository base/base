use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    BlockRange, ConfigSummary, FlashblocksLatencyMetrics, GasMetrics, LatencyMetrics,
    ObservedWindowMetrics, SubmissionStats, TailMetrics, ThroughputMetrics, ThroughputPercentiles,
    ThroughputSample, TransactionMetrics, types::BLOCK_INTERVAL,
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
    ///
    /// `wall_clock_duration` is used as a fallback for TPS when block timestamps
    /// are unavailable. When block timestamps are present, TPS is derived from
    /// the first-to-last block time span instead.
    ///
    /// `configured_duration` is the user-configured test duration (e.g. `60s`
    /// from `--duration 60s`). When provided, it anchors the observed-window
    /// boundary and the "tail" (post-observed-window) classification. When
    /// `None` (continuous run), `wall_clock_duration` is used for the observed
    /// window and `tail` is reported as `None`.
    pub fn summarize(
        &self,
        wall_clock_duration: Duration,
        configured_duration: Option<Duration>,
        submission: SubmissionStats<'_>,
        throughput_samples: &[ThroughputSample],
        config: Option<ConfigSummary>,
    ) -> MetricsSummary {
        let mut top_failure_reasons: Vec<(String, u64)> =
            submission.failure_reasons.iter().map(|(k, v)| (k.clone(), *v)).collect();
        top_failure_reasons.sort_by(|a, b| b.1.cmp(&a.1));
        top_failure_reasons.truncate(3);

        let tps_values: Vec<f64> = throughput_samples.iter().map(|s| s.tps).collect();
        let gps_values: Vec<f64> = throughput_samples.iter().map(|s| s.gps).collect();

        let block_range = Self::compute_block_range(self.transactions);
        let throughput_duration = block_range.block_time_duration().unwrap_or(wall_clock_duration);
        let reference_duration = configured_duration.unwrap_or(wall_clock_duration);

        let observed_window = Self::compute_observed_window(
            self.transactions,
            reference_duration,
            block_range.first_block,
        );
        let tail = configured_duration.map(|_| {
            Self::compute_tail(
                self.transactions,
                observed_window.expected_block_count,
                block_range.first_block,
            )
        });

        MetricsSummary {
            config,
            error: None,
            observed_window,
            tail,
            block_latency: Self::compute_block_latency(self.transactions),
            block_receipt_delay: Self::compute_block_receipt_delay(self.transactions),
            flashblocks_latency: Self::compute_flashblocks_latency(self.transactions),
            throughput: Self::compute_throughput(
                self.transactions,
                throughput_duration,
                submission.submitted,
                submission.failed,
            ),
            throughput_percentiles: Self::compute_throughput_percentiles(&tps_values, &gps_values),
            throughput_timeseries: throughput_samples.to_vec(),
            gas: Self::compute_gas(self.transactions),
            block_range,
            top_failure_reasons,
        }
    }

    /// Computes the observed reporting window:
    /// `expected_block_count = reference_duration.as_secs() / BLOCK_INTERVAL.as_secs()`,
    /// starting at `first_block`. TPS / GPS denominator = `expected_block_count *
    /// BLOCK_INTERVAL` (real L2 wall-time spanned by those expected blocks).
    ///
    /// `first_block` is taken from the caller's pre-computed full block range
    /// to avoid re-scanning `transactions`.
    fn compute_observed_window(
        transactions: &[TransactionMetrics],
        reference_duration: Duration,
        first_block: Option<u64>,
    ) -> ObservedWindowMetrics {
        let expected_block_count = reference_duration.as_secs() / BLOCK_INTERVAL.as_secs();
        let duration =
            Duration::from_secs(expected_block_count.saturating_mul(BLOCK_INTERVAL.as_secs()));
        let Some(first_block) = first_block else {
            return ObservedWindowMetrics {
                expected_block_count,
                duration,
                ..ObservedWindowMetrics::default()
            };
        };

        let end_block = first_block.saturating_add(expected_block_count.saturating_sub(1));
        let in_window = |t: &&TransactionMetrics| t.block_number.is_some_and(|b| b <= end_block);

        let (confirmed_count, total_gas) = transactions
            .iter()
            .filter(in_window)
            .fold((0u64, 0u64), |(n, gas), t| (n + 1, gas + t.gas_used));
        let duration_secs = duration.as_secs_f64();
        let (tps, gps) = if duration_secs > 0.0 {
            (confirmed_count as f64 / duration_secs, total_gas as f64 / duration_secs)
        } else {
            (0.0, 0.0)
        };

        ObservedWindowMetrics {
            expected_block_count,
            block_range: Self::compute_block_range(transactions.iter().filter(in_window)),
            duration,
            confirmed_count,
            tps,
            gps,
            block_latency: Self::compute_block_latency(transactions.iter().filter(in_window)),
            block_receipt_delay: Self::compute_block_receipt_delay(
                transactions.iter().filter(in_window),
            ),
            flashblocks_latency: Self::compute_flashblocks_latency(
                transactions.iter().filter(in_window),
            ),
        }
    }

    /// Computes the inclusion-delay tail: transactions whose block number is
    /// strictly greater than the observed-window end block
    /// (`first_block + observed_window_expected_block_count - 1`). Captures
    /// straggler receipts that landed past the clean reporting window.
    fn compute_tail(
        transactions: &[TransactionMetrics],
        observed_window_expected_block_count: u64,
        first_block: Option<u64>,
    ) -> TailMetrics {
        let Some(first_block) = first_block else {
            return TailMetrics::default();
        };
        let total_confirmed = transactions.len() as u64;

        let observed_window_end_block =
            first_block.saturating_add(observed_window_expected_block_count.saturating_sub(1));
        let is_tail =
            |t: &&TransactionMetrics| t.block_number.is_some_and(|b| b > observed_window_end_block);

        let count = transactions.iter().filter(is_tail).count() as u64;
        let confirmed_pct =
            if total_confirmed > 0 { (count as f64 / total_confirmed as f64) * 100.0 } else { 0.0 };

        let mut time_past: Vec<Duration> = transactions
            .iter()
            .filter(is_tail)
            .filter_map(|t| t.block_number)
            .map(|b| BLOCK_INTERVAL * (b - observed_window_end_block) as u32)
            .collect();

        TailMetrics {
            observed_window_end_block: Some(observed_window_end_block),
            count,
            confirmed_pct,
            block_range: Self::compute_block_range(transactions.iter().filter(is_tail)),
            time_past_observed_window: Self::compute_duration_metrics(&mut time_past),
            block_latency: Self::compute_block_latency(transactions.iter().filter(is_tail)),
            block_receipt_delay: Self::compute_block_receipt_delay(
                transactions.iter().filter(is_tail),
            ),
            flashblocks_latency: Self::compute_flashblocks_latency(
                transactions.iter().filter(is_tail),
            ),
        }
    }

    fn compute_block_range<'t>(
        transactions: impl IntoIterator<Item = &'t TransactionMetrics>,
    ) -> BlockRange {
        let mut iter = transactions.into_iter().filter_map(|t| t.block_number);
        let Some(first) = iter.next() else {
            return BlockRange::default();
        };
        let (min, max) = iter.fold((first, first), |(lo, hi), b| (lo.min(b), hi.max(b)));
        BlockRange { first_block: Some(min), last_block: Some(max), block_count: max - min + 1 }
    }

    fn compute_block_latency<'t>(
        transactions: impl IntoIterator<Item = &'t TransactionMetrics>,
    ) -> LatencyMetrics {
        let mut latencies: Vec<Duration> =
            transactions.into_iter().filter_map(|t| t.block_latency).collect();

        Self::compute_duration_metrics(&mut latencies)
    }

    fn compute_block_receipt_delay<'t>(
        transactions: impl IntoIterator<Item = &'t TransactionMetrics>,
    ) -> LatencyMetrics {
        let mut latencies: Vec<Duration> =
            transactions.into_iter().filter_map(|t| t.block_receipt_delay).collect();

        Self::compute_duration_metrics(&mut latencies)
    }

    fn compute_duration_metrics(latencies: &mut [Duration]) -> LatencyMetrics {
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
            p50: Self::percentile(latencies, 50),
            p95: Self::percentile(latencies, 95),
            p99: Self::percentile(latencies, 99),
        }
    }

    fn compute_flashblocks_latency<'t>(
        transactions: impl IntoIterator<Item = &'t TransactionMetrics>,
    ) -> FlashblocksLatencyMetrics {
        let mut latencies: Vec<Duration> =
            transactions.into_iter().filter_map(|t| t.flashblocks_latency).collect();

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
        transactions: &[TransactionMetrics],
        duration: Duration,
        submitted: u64,
        failed: u64,
    ) -> ThroughputMetrics {
        let confirmed = transactions.len() as u64;
        let total_reverted = transactions.iter().filter(|t| t.reverted).count() as u64;
        let total_gas: u64 = transactions.iter().map(|t| t.gas_used).sum();
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
            total_reverted,
            tps,
            gps,
            duration,
        }
    }

    fn compute_gas(transactions: &[TransactionMetrics]) -> GasMetrics {
        if transactions.is_empty() {
            return GasMetrics::default();
        }

        let total_gas: u64 = transactions.iter().map(|t| t.gas_used).sum();
        let total_cost: u128 = transactions.iter().map(|t| t.cost_wei()).sum();
        let total_gas_price: u128 = transactions.iter().map(|t| t.gas_price).sum();
        let count = transactions.len() as u64;

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
///
/// Reporting is split into two diagnostic scopes:
/// * `observed_window` covers the clean reporting window of the configured
///   run and is used for headline TPS / latency comparisons.
/// * `tail` quantifies the inclusion delay: transactions that landed in
///   blocks past the configured submission window. `None` for continuous runs.
///
/// Top-level fields (`throughput`, `block_latency`, `gas`, `block_range`,
/// `top_failure_reasons`, etc.) are baseline full-run accounting, not headline
/// metrics — full-run TPS/latency is misleading because it averages the clean
/// window with the tail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSummary {
    /// Test configuration (excludes URLs and secrets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigSummary>,
    /// Fatal error that stopped the test (e.g., funding failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// First-half reporting window (clean TPS / block / FB latency).
    pub observed_window: ObservedWindowMetrics,
    /// Inclusion-delay tail (txs landing after the configured submission
    /// window). `None` when the run had no configured duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<TailMetrics>,
    /// Block production latency (full run, baseline accounting).
    pub block_latency: LatencyMetrics,
    /// Delay between block production time and receipt observation (full run).
    pub block_receipt_delay: LatencyMetrics,
    /// Flashblocks sequencer latency (full run, baseline accounting).
    pub flashblocks_latency: FlashblocksLatencyMetrics,
    /// Throughput statistics (full run, baseline accounting).
    pub throughput: ThroughputMetrics,
    /// Rolling-window throughput percentiles (TPS and GPS).
    pub throughput_percentiles: ThroughputPercentiles,
    /// Throughput samples over time for graphing.
    pub throughput_timeseries: Vec<ThroughputSample>,
    /// Gas usage statistics.
    pub gas: GasMetrics,
    /// Range of blocks containing confirmed test transactions.
    pub block_range: BlockRange,
    /// Top failure reasons sorted by count descending (max 3).
    pub top_failure_reasons: Vec<(String, u64)>,
}

impl MetricsSummary {
    /// Serializes the summary to JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}
