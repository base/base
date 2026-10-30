//! Per-block Prometheus metric collection for benchmarked execution nodes.

use std::{collections::BTreeMap, time::Duration};

use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use eyre::{Result, WrapErr};
use prometheus_scraper::{
    Format, TextFormat,
    borrowed::{BucketCount, LabelPair, MetricValue},
    owned::{NativeCounts, Number},
    parse_payload,
};
use tokio::{sync::watch, task::JoinHandle};
use url::Url;

/// Duration histograms configured as cumulative buckets for benchmark-window quantiles.
pub const WINDOW_HISTOGRAMS: &[&str] = &[
    "reth_base_builder_native_payload_build_duration",
    "reth_consensus_engine_beacon_new_payload_latency",
    "reth_consensus_engine_beacon_block_insert_total_duration",
    "reth_engine_rpc_get_payload_v1", "reth_engine_rpc_get_payload_v2",
    "reth_engine_rpc_get_payload_v3", "reth_engine_rpc_get_payload_v4",
    "reth_engine_rpc_get_payload_v5", "reth_engine_rpc_get_payload_v6",
];

/// Samples of executed blocks retained by the engine, not a persisted-head height.
pub const WINDOW_GAUGE: &str = "reth_consensus_engine_beacon_executed_blocks";

/// Prometheus metrics rendered by the snapshot benchmark visualizer.
const VISUALIZER_METRICS: &[&str] = &[
    "reth_base_builder_flashblock_build_duration_avg",
    "reth_base_builder_flashblock_count",
    "reth_base_builder_native_payload_build_duration_avg",
    "reth_base_builder_native_payload_execution_duration_avg",
    "reth_base_builder_native_payload_state_root_duration_avg",
    "reth_base_builder_payload_num_tx_gauge",
    "reth_base_builder_predicate_accounts_loaded_total_avg",
    "reth_base_builder_predicate_accounts_loaded_unique_avg",
    "reth_base_builder_predicate_bucket_depth_avg",
    "reth_base_builder_predicate_bucket_wakeups_avg",
    "reth_base_builder_predicate_slots_loaded_total_avg",
    "reth_base_builder_predicate_slots_loaded_unique_avg",
    "reth_base_builder_resting_predicate_carried_avg",
    "reth_base_builder_resting_predicate_carried_parked_total",
    "reth_base_builder_resting_predicate_carry_released_total_reason_block_number",
    "reth_base_builder_resting_predicate_carry_released_total_reason_pre_block_state",
    "reth_base_builder_resting_predicate_carry_released_total_reason_unseen",
    "reth_base_builder_resting_predicate_carry_resets_total",
    "reth_base_builder_resting_predicate_parked_total",
    "reth_base_builder_resting_predicate_shadow_mismatches_total",
    "reth_base_builder_payload_transaction_simulation_duration_avg",
    "reth_base_builder_sequencer_tx_duration_avg",
    "reth_base_builder_state_root_calculation_duration_avg",
    "reth_base_builder_total_block_built_duration_avg",
    "reth_base_builder_tx_simulation_duration_avg",
    "reth_base_builder_tx_gas_used_per_block_flow_standard_avg",
    "reth_base_builder_tx_gas_used_per_block_flow_validity_avg",
    "reth_base_builder_txs_included_per_block_flow_standard_avg",
    "reth_base_builder_txs_included_per_block_flow_validity_avg",
    "reth_base_builder_validity_candidate_handling_duration_avg",
    "reth_base_builder_validity_predicate_candidates_deferred_per_build_avg",
    "reth_base_builder_validity_predicate_candidates_evaluated_per_build_avg",
    "reth_base_builder_validity_predicate_eval_cutoff_builds_total",
    "reth_base_builder_validity_predicate_eval_duration_per_block_avg",
    "reth_base_builder_validity_predicate_evaluation_coverage_per_build_avg",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_budget_exhausted",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_expired",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_matched",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_not_satisfied",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_read_error",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_rescan_budget_exhausted",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_rescan_matched",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_rescan_not_satisfied",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_rescan_read_error",
    "reth_base_builder_validity_predicate_evaluations_total_outcome_unsupported",
    "reth_base_builder_validity_predicate_rescan_duration_avg",
    "reth_consensus_engine_beacon_backpressure_active",
    "reth_consensus_engine_beacon_block_insert_total_duration_avg",
    "reth_consensus_engine_beacon_new_payload_latency_avg",
    "reth_consensus_engine_persistence_save_blocks_batch_size_avg",
    "reth_consensus_engine_persistence_save_blocks_duration_seconds_avg",
    "reth_db_freelist",
    "reth_op_rbuilder_flashblock_build_duration",
    "reth_op_rbuilder_flashblock_count",
    "reth_op_rbuilder_payload_tx_simulation_duration",
    "reth_op_rbuilder_sequencer_tx_duration",
    "reth_op_rbuilder_state_root_calculation_duration",
    "reth_op_rbuilder_total_block_built_duration",
    "reth_sync_block_validation_deferred_trie_compute_duration_avg",
    "reth_sync_block_validation_hashed_post_state_size_avg",
    "reth_sync_block_validation_state_root_duration",
    "reth_sync_block_validation_state_root_duration_avg",
    "reth_sync_block_validation_total_duration_avg",
    "reth_sync_block_validation_trie_updates_sorted_size_avg",
    "reth_sync_execution_accounts_updated_histogram_avg",
    "reth_sync_execution_execution_duration",
    "reth_sync_execution_execution_duration_avg",
    "reth_sync_execution_storage_slots_updated_histogram_avg",
    "reth_sync_execution_transaction_execution_histogram_avg",
    "reth_sync_execution_transaction_wait_histogram_avg",
    "reth_sync_state_provider_total_account_fetch_latency",
    "reth_sync_state_provider_total_account_fetch_latency_avg",
    "reth_sync_state_provider_total_code_fetch_latency",
    "reth_sync_state_provider_total_code_fetch_latency_avg",
    "reth_sync_state_provider_total_storage_fetch_latency",
    "reth_sync_state_provider_total_storage_fetch_latency_avg",
    "reth_transaction_pool_pending_pool_transactions",
    "reth_transaction_pool_total_transactions",
    "reth_tree_root_sparse_trie_cache_wait_duration_histogram_avg",
    "reth_tree_root_sparse_trie_channel_wait_duration_histogram_avg",
    "reth_tree_root_sparse_trie_final_update_duration_histogram",
    "reth_tree_root_sparse_trie_final_update_duration_histogram_avg",
    "reth_tree_root_sparse_trie_reveal_multiproof_duration_histogram_avg",
    "reth_tree_root_sparse_trie_total_duration_histogram",
    "reth_tree_root_sparse_trie_total_duration_histogram_avg",
    "reth_txpool_validity_admitted_outcome_added",
    "reth_txpool_validity_admitted_outcome_replaced",
];

#[derive(Debug, Clone, Copy)]
enum ScalarSample {
    Counter(f64),
    Gauge(f64),
}

#[derive(Debug, Clone, Default)]
struct DistributionSample {
    sum: Option<f64>,
    count: Option<f64>,
    /// Cumulative classic histogram counts sorted by upper bound, including +Inf.
    buckets: Vec<(f64, f64)>,
}

/// Parsed values from one Prometheus endpoint scrape.
#[derive(Debug, Clone, Default)]
pub struct PrometheusSnapshot {
    scalars: BTreeMap<String, ScalarSample>,
    distributions: BTreeMap<String, DistributionSample>,
}

impl PrometheusSnapshot {
    /// Installs cumulative duration buckets before nodes install Reth's default rolling summaries.
    /// Bucket edges are seconds; interpolation cannot recover precision within each bucket.
    pub fn install_window_recorder() -> Result<()> {
        let builder = Self::window_histogram_builder()?;
        reth_node_metrics::recorder::try_install_prometheus_recorder_with_builder(builder)?;
        Ok(())
    }

    /// Configures only experiment duration metrics as histograms, preserving other instruments.
    pub fn window_histogram_builder() -> Result<metrics_exporter_prometheus::PrometheusBuilder> {
        let mut builder = metrics_exporter_prometheus::PrometheusBuilder::new();
        for name in WINDOW_HISTOGRAMS {
            let suffix = name.strip_prefix("reth_engine_rpc_").unwrap_or_else(|| {
                name.strip_prefix("reth_base_builder_").unwrap_or_else(||
                    name.strip_prefix("reth_consensus_engine_beacon_").unwrap_or(name))
            });
            builder = builder.set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Suffix(suffix.to_owned()),
                &[0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1.0, 2.0, 5.0, 10.0],
            )?;
        }
        Ok(builder)
    }

    /// Parses Prometheus text exposition into numeric samples.
    pub fn parse(input: &str) -> Self {
        let mut scalars = BTreeMap::new();
        let mut distributions = BTreeMap::new();
        for family in parse_payload(input.as_bytes(), Format::Text(TextFormat::Prometheus)) {
            let Ok(family) = family else {
                continue;
            };
            let family_name = family.name.as_ref();
            for metric in family.metric {
                let key = Self::sample_key(family_name, &metric.label);
                match metric.value {
                    MetricValue::Counter(counter) => {
                        Self::insert_scalar(
                            &mut scalars,
                            key,
                            ScalarSample::Counter(counter.value.as_f64()),
                        );
                    }
                    MetricValue::Gauge(value) | MetricValue::Untyped(value) => {
                        Self::insert_scalar(&mut scalars, key, ScalarSample::Gauge(value.as_f64()));
                    }
                    MetricValue::Histogram(histogram) | MetricValue::GaugeHistogram(histogram) => {
                        Self::insert_buckets(&mut distributions, &key, &histogram.counts);
                        Self::insert_distribution(
                            &mut distributions,
                            key,
                            histogram.sample_sum,
                            Self::bucket_count(&histogram.counts),
                        )
                    }
                    MetricValue::Summary(summary) => Self::insert_distribution(
                        &mut distributions,
                        key,
                        summary.sample_sum,
                        summary.sample_count.map(|count| count as f64),
                    ),
                    MetricValue::NativeHistogram(histogram) => Self::insert_distribution(
                        &mut distributions,
                        key,
                        histogram.sample_sum,
                        Self::native_count(&histogram.counts),
                    ),
                    MetricValue::HybridHistogram { classic, .. } => {
                        Self::insert_buckets(&mut distributions, &key, &classic.counts);
                        Self::insert_distribution(
                            &mut distributions, key, classic.sample_sum,
                            Self::bucket_count(&classic.counts),
                        );
                    }
                    MetricValue::StateSet(_) | MetricValue::Info(_) => {}
                }
            }
        }
        Self { scalars, distributions }
    }

    /// Produces report-ready gauges and per-scrape counter/histogram deltas.
    pub fn delta(&self, previous: &Self) -> BTreeMap<String, f64> {
        self.delta_for_blocks(previous, 1)
    }

    /// Produces per-block values when one scrape spans multiple canonical blocks.
    pub fn delta_for_blocks(&self, previous: &Self, block_count: u64) -> BTreeMap<String, f64> {
        let divisor = block_count.max(1) as f64;
        let mut metrics = BTreeMap::new();
        for (key, sample) in &self.scalars {
            let value = match sample {
                ScalarSample::Counter(value) => {
                    let previous = previous.scalars.get(key).and_then(|sample| match sample {
                        ScalarSample::Counter(value) => Some(value),
                        ScalarSample::Gauge(_) => None,
                    });
                    Self::counter_delta(*value, previous) / divisor
                }
                ScalarSample::Gauge(value) => *value,
            };
            metrics.insert(key.clone(), value);
        }

        for (key, sample) in &self.distributions {
            let baseline = previous.distributions.get(key);
            let valid_buckets = baseline.is_none_or(|before| before.buckets.is_empty() ||
                (before.buckets.len() == sample.buckets.len() &&
                    before.buckets.iter().zip(&sample.buckets).all(|(old, new)|
                        old.0 == new.0 && old.1 <= new.1)));
            for (bound, count) in sample.buckets.iter().filter(|_| valid_buckets) {
                let prior = previous.distributions.get(key).and_then(|sample| sample.buckets.iter()
                    .find(|(edge, _)| edge == bound).map(|(_, count)| count));
                metrics.insert(format!("benchmark/histogram/{key}/{bound}"),
                    Self::counter_delta(*count, prior) / divisor);
            }
            let (Some(sum), Some(count)) = (sample.sum, sample.count) else {
                continue;
            };
            let previous = previous.distributions.get(key);
            let sum_delta =
                Self::counter_delta(sum, previous.and_then(|sample| sample.sum.as_ref()));
            let count_delta =
                Self::counter_delta(count, previous.and_then(|sample| sample.count.as_ref()));
            if count_delta > 0.0 {
                metrics.insert(format!("{key}_avg"), sum_delta / count_delta);
            }
        }
        metrics
    }

    /// Interpolates classic histograms using Prometheus `histogram_quantile` semantics.
    /// Precision is bounded by bucket edges; +Inf ranks use the highest finite edge.
    /// Missing +Inf, empty windows and invalid counts yield no value.
    pub fn histogram_quantile(buckets: &[(f64, f64)], quantile: f64) -> Option<f64> {
        let &(last, total) = buckets.last()?;
        if !quantile.is_finite() || !(0.0..=1.0).contains(&quantile) { return None; }
        if buckets.len() < 2 || last != f64::INFINITY || total <= 0.0 || !total.is_finite() {
            return None;
        }
        let rank = total * quantile;
        let mut lower = 0.0;
        let mut previous = 0.0;
        for &(upper, count) in buckets {
            if upper.is_nan() || !count.is_finite() || count < previous { return None; }
            if count >= rank {
                if upper == f64::INFINITY { return Some(lower); }
                if count == previous || upper <= 0.0 { return Some(upper); }
                return Some(lower + (upper - lower) * (rank - previous) / (count - previous));
            }
            lower = upper;
            previous = count;
        }
        None
    }

    /// Keeps classic buckets. Summary quantiles are not subtracted: their rolling windows
    /// cannot be reconstructed from two scrapes, so only their existing average is retained.
    fn insert_buckets(
        distributions: &mut BTreeMap<String, DistributionSample>,
        key: &str,
        counts: &BucketCount<'_>,
    ) {
        let mut buckets: Vec<_> = match counts {
            BucketCount::Int { buckets, .. } => buckets.iter()
                .map(|bucket| (bucket.upper_bound, bucket.cumulative_count as f64)).collect(),
            BucketCount::Float { buckets, .. } => buckets.iter()
                .map(|bucket| (bucket.upper_bound, bucket.cumulative_count)).collect(),
        };
        buckets.sort_by(|left, right| left.0.total_cmp(&right.0));
        distributions.entry(key.to_owned()).or_default().buckets = buckets;
    }

    fn insert_scalar(
        scalars: &mut BTreeMap<String, ScalarSample>,
        key: String,
        sample: ScalarSample,
    ) {
        let value = match sample {
            ScalarSample::Counter(value) | ScalarSample::Gauge(value) => value,
        };
        if value.is_finite() {
            scalars.insert(key, sample);
        }
    }

    fn insert_distribution(
        distributions: &mut BTreeMap<String, DistributionSample>,
        key: String,
        sum: Option<Number>,
        count: Option<f64>,
    ) {
        let entry = distributions.entry(key).or_default();
        if let Some(sum) = sum.map(Number::as_f64).filter(|value| value.is_finite()) {
            entry.sum = Some(sum);
        }
        if let Some(count) = count.filter(|value| value.is_finite()) {
            entry.count = Some(count);
        }
    }

    fn bucket_count(counts: &BucketCount<'_>) -> Option<f64> {
        match counts {
            BucketCount::Int { sample_count, .. } => sample_count.map(|count| count as f64),
            BucketCount::Float { sample_count, .. } => *sample_count,
        }
    }

    fn native_count(counts: &NativeCounts) -> Option<f64> {
        match counts {
            NativeCounts::Int { sample_count, .. } => sample_count.map(|count| count as f64),
            NativeCounts::Float { sample_count, .. } => *sample_count,
        }
    }

    fn sample_key(name: &str, labels: &[LabelPair<'_>]) -> String {
        let mut labels = labels
            .iter()
            .filter(|label| label.name != "le" && label.name != "quantile")
            .map(|label| {
                format!("{}_{}", Self::sanitize(&label.name), Self::sanitize(&label.value))
            })
            .collect::<Vec<_>>();
        labels.sort();
        if labels.is_empty() { name.to_string() } else { format!("{name}_{}", labels.join("_")) }
    }

    fn sanitize(value: &str) -> String {
        value
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
            .collect()
    }

    fn counter_delta(value: f64, previous: Option<&f64>) -> f64 {
        previous.filter(|previous| value >= **previous).map_or(value, |previous| value - previous)
    }
}

/// Background collector that records one Prometheus delta snapshot per observed canonical block.
#[derive(Debug)]
pub struct PrometheusBlockCollector {
    end_sender: watch::Sender<Option<u64>>,
    task: JoinHandle<Result<BTreeMap<u64, BTreeMap<String, f64>>>>,
}

impl PrometheusBlockCollector {
    /// Starts polling an execution RPC head and scraping its Prometheus endpoint at most once per
    /// `scrape_interval`.
    pub async fn start(rpc_url: Url, metrics_url: Url, scrape_interval: Duration) -> Result<Self> {
        let provider = RootProvider::<Base>::new_http(rpc_url);
        let client = reqwest::Client::new();
        let initial_head = provider.get_block_number().await?;
        let initial = Self::scrape(&client, &metrics_url).await?;
        let (end_sender, mut end_receiver) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut previous = initial;
            let mut last_head = initial_head;
            let mut samples = BTreeMap::new();
            loop {
                let head = provider.get_block_number().await?;
                if head > last_head {
                    let current = Self::scrape(&client, &metrics_url).await?;
                    let block_count = head - last_head;
                    let mut metrics = current.delta_for_blocks(&previous, block_count);
                    metrics.retain(|name, _| Self::is_visualizer_metric(name));
                    metrics.insert(
                        "benchmark/prometheus_blocks_per_scrape".to_string(),
                        block_count as f64,
                    );
                    for block in last_head.saturating_add(1)..=head {
                        samples.insert(block, metrics.clone());
                    }
                    previous = current;
                    last_head = head;
                }
                if end_receiver.borrow().is_some_and(|end| last_head >= end) {
                    return Ok(samples);
                }
                tokio::select! {
                    // Rendering Reth's full endpoint is expensive enough to perturb 200ms block
                    // production when requested continuously, so callers bound the scrape rate.
                    // Canonical block totals remain exact; diagnostic counters are explicitly
                    // attributed across this scrape interval.
                    _ = tokio::time::sleep(scrape_interval) => {}
                    changed = end_receiver.changed() => {
                        changed.wrap_err("metric collector completion channel closed")?;
                    }
                }
            }
        });
        Ok(Self { end_sender, task })
    }

    /// Finishes after collecting the requested final block and returns samples keyed by block.
    pub async fn finish(self, end_block: u64) -> Result<BTreeMap<u64, BTreeMap<String, f64>>> {
        self.end_sender
            .send(Some(end_block))
            .wrap_err("metric collector stopped before receiving its end block")?;
        tokio::time::timeout(Duration::from_secs(30 * 60), self.task)
            .await
            .wrap_err("timed out waiting for per-block Prometheus metrics")?
            .wrap_err("per-block Prometheus collector task failed")?
    }

    /// Fetches and parses one Prometheus endpoint response.
    pub async fn scrape(client: &reqwest::Client, metrics_url: &Url) -> Result<PrometheusSnapshot> {
        let response = client
            .get(metrics_url.clone())
            .send()
            .await
            .wrap_err_with(|| format!("failed to scrape Prometheus endpoint {metrics_url}"))?
            .error_for_status()?;
        Ok(PrometheusSnapshot::parse(&response.text().await?))
    }

    /// Returns whether a flattened metric is rendered by the benchmark visualizer.
    ///
    /// This intentionally uses exact metric names rather than broad metric-family prefixes. The
    /// latter pull in one sample for every flashblock, gas bucket, and label combination, even
    /// though the report UI cannot display them. Keep this list in sync with the metrics rendered
    /// by `base/benchmark`'s `ChartGrid`, including its compatibility aliases.
    pub fn is_visualizer_metric(name: &str) -> bool {
        VISUALIZER_METRICS.contains(&name) || name == WINDOW_GAUGE
            || WINDOW_HISTOGRAMS.iter().any(|metric|
                name.starts_with(&format!("benchmark/histogram/{metric}/")))
    }
}

#[cfg(test)]
mod tests {
    use super::{PrometheusBlockCollector, PrometheusSnapshot};

    /// Exporter configuration produces real classic buckets, not rolling summary quantiles.
    #[test]
    fn duration_exporter_uses_classic_histograms() {
        let recorder = PrometheusSnapshot::window_histogram_builder().unwrap().build_recorder();
        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("reth.base_builder.native_payload_build_duration").record(0.1);
            metrics::histogram!("reth.engine.rpc.get_payload_v4").record(0.2);
        });
        let text = recorder.handle().render();
        for name in ["reth_base_builder_native_payload_build_duration", "reth_engine_rpc_get_payload_v4"] {
            assert!(text.contains(&format!("# TYPE {name} histogram")), "{name} must have cumulative buckets");
            assert!(text.contains(&format!("{name}_bucket")), "{name} must expose bucket counts");
            assert!(!text.contains(&format!("{name}{{quantile=")), "{name} must not use rolling summaries");
        }
    }

    /// Cumulative bucket deltas preserve the old average and interpolate window tails.
    #[test]
    fn histogram_window_deltas_preserve_average_and_tails() {
        let before = PrometheusSnapshot::parse("# TYPE build histogram\nbuild_bucket{le=\"1\"} 2\nbuild_bucket{le=\"2\"} 4\nbuild_bucket{le=\"+Inf\"} 4\nbuild_sum 4\nbuild_count 4\n");
        let after = PrometheusSnapshot::parse("# TYPE build histogram\nbuild_bucket{le=\"1\"} 4\nbuild_bucket{le=\"2\"} 8\nbuild_bucket{le=\"+Inf\"} 8\nbuild_sum 10\nbuild_count 8\n");
        let delta = after.delta(&before);
        assert_eq!(delta["build_avg"], 1.5, "legacy sum/count average stays unchanged");
        assert_eq!(delta["benchmark/histogram/build/1"], 2.0, "finite bucket counts are subtracted");
        assert_eq!(delta["benchmark/histogram/build/inf"], 4.0, "infinite bucket counts are subtracted");
        assert_eq!(PrometheusSnapshot::histogram_quantile(&[(1.0, 2.0), (2.0, 4.0), (f64::INFINITY, 4.0)], 0.99), Some(1.98), "p99 interpolates bucket delta rather than averages");
    }

    /// Rolling summary quantiles cannot become window quantiles by subtraction.
    #[test]
    fn summary_quantiles_remain_average_only() {
        let summary = PrometheusSnapshot::parse("# TYPE duration summary\nduration{quantile=\"0.99\"} 10\nduration_sum 6\nduration_count 2\n");
        let delta = summary.delta(&PrometheusSnapshot::default());
        assert_eq!(delta["duration_avg"], 3.0, "summary sum/count average is preserved");
        assert_eq!(delta.len(), 1, "summary quantiles must not become fake window percentiles");
    }

    /// Prometheus returns the highest finite bound for ranks in +Inf and omits empty windows.
    #[test]
    fn histogram_quantile_infinite_and_empty_windows() {
        let buckets = [(1.0, 2.0), (2.0, 4.0), (f64::INFINITY, 8.0)];
        assert_eq!(PrometheusSnapshot::histogram_quantile(&buckets, 0.5), Some(2.0), "p50 is the finite edge");
        assert_eq!(PrometheusSnapshot::histogram_quantile(&buckets, 0.95), Some(2.0), "p95 in infinity uses highest finite edge");
        assert_eq!(PrometheusSnapshot::histogram_quantile(&[], 0.99), None, "missing buckets yield no percentile");
        assert_eq!(PrometheusSnapshot::histogram_quantile(&[(1.0, 0.0), (f64::INFINITY, 0.0)], 0.99), None, "empty window yields no percentile");
    }

    #[test]
    fn computes_counter_and_histogram_deltas() {
        let previous = PrometheusSnapshot::parse(
            "# TYPE requests counter\nrequests 10\n\
             # TYPE build_duration histogram\nbuild_duration_sum 4\nbuild_duration_count 2\n",
        );
        let current = PrometheusSnapshot::parse(
            "# TYPE requests counter\nrequests 15\n\
             # TYPE build_duration histogram\nbuild_duration_sum 10\nbuild_duration_count 4\n\
             # TYPE queue gauge\nqueue 7\n",
        );
        let delta = current.delta(&previous);
        assert_eq!(delta["requests"], 5.0);
        assert_eq!(delta["build_duration_avg"], 3.0);
        assert_eq!(delta["queue"], 7.0);
    }

    #[test]
    fn averages_counter_deltas_across_skipped_blocks() {
        let previous = PrometheusSnapshot::parse(
            "# TYPE requests counter\nrequests 10\n\
             # TYPE build_duration histogram\nbuild_duration_sum 4\nbuild_duration_count 2\n",
        );
        let current = PrometheusSnapshot::parse(
            "# TYPE requests counter\nrequests 16\n\
             # TYPE build_duration histogram\nbuild_duration_sum 10\nbuild_duration_count 4\n\
             # TYPE queue gauge\nqueue 7\n",
        );
        let delta = current.delta_for_blocks(&previous, 2);
        assert_eq!(delta["requests"], 3.0);
        assert_eq!(delta["build_duration_avg"], 3.0);
        assert_eq!(delta["queue"], 7.0);
    }

    #[test]
    fn preserves_non_bucket_labels_in_metric_keys() {
        let snapshot = PrometheusSnapshot::parse(
            "# TYPE rejected counter\nrejected{reason=\"gas limit\",le=\"1\"} 3\n",
        );
        let delta = snapshot.delta(&PrometheusSnapshot::default());
        assert_eq!(delta["rejected_reason_gas_limit"], 3.0);
    }

    #[test]
    fn computes_labeled_histogram_averages() {
        let previous = PrometheusSnapshot::parse(
            r#"# TYPE build_duration histogram
build_duration_sum{stage="execution"} 4
build_duration_count{stage="execution"} 2
"#,
        );
        let current = PrometheusSnapshot::parse(
            r#"# TYPE build_duration histogram
build_duration_sum{stage="execution"} 10
build_duration_count{stage="execution"} 4
"#,
        );
        let delta = current.delta(&previous);
        assert_eq!(delta["build_duration_stage_execution_avg"], 3.0);
    }

    #[test]
    fn selects_only_metrics_rendered_by_the_visualizer() {
        assert!(PrometheusBlockCollector::is_visualizer_metric(
            "reth_base_builder_total_block_built_duration_avg"
        ));
        assert!(PrometheusBlockCollector::is_visualizer_metric(
            "reth_sync_execution_execution_duration"
        ));
        assert!(PrometheusBlockCollector::is_visualizer_metric(
            "reth_consensus_engine_beacon_new_payload_latency_avg"
        ));
        assert!(PrometheusBlockCollector::is_visualizer_metric(
            "reth_tree_root_sparse_trie_final_update_duration_histogram"
        ));
        assert!(PrometheusBlockCollector::is_visualizer_metric(
            "reth_op_rbuilder_flashblock_count"
        ));
        assert!(!PrometheusBlockCollector::is_visualizer_metric(
            "reth_base_builder_flashblock_gas_headroom_flashblock_index_1_avg"
        ));
        assert!(!PrometheusBlockCollector::is_visualizer_metric(
            "reth_consensus_engine_beacon_new_payload_thread_user_cpu_seconds_avg"
        ));
        assert!(!PrometheusBlockCollector::is_visualizer_metric(
            "reth_rpc_server_calls_started_total_method_eth_call"
        ));
    }

    #[test]
    fn collects_native_payload_builder_metrics() {
        let current = PrometheusSnapshot::parse(
            r#"# TYPE reth_base_builder_native_payload_build_duration summary
reth_base_builder_native_payload_build_duration{quantile="0.5"} 0.01
reth_base_builder_native_payload_build_duration_sum 0.25
reth_base_builder_native_payload_build_duration_count 5
# TYPE reth_base_builder_txs_included_per_block summary
reth_base_builder_txs_included_per_block_sum{flow="standard"} 650
reth_base_builder_txs_included_per_block_count{flow="standard"} 5
"#,
        );
        let mut delta = current.delta_for_blocks(&PrometheusSnapshot::default(), 5);
        delta.retain(|name, _| PrometheusBlockCollector::is_visualizer_metric(name));
        assert_eq!(delta["reth_base_builder_native_payload_build_duration_avg"], 0.05);
        assert_eq!(delta["reth_base_builder_txs_included_per_block_flow_standard_avg"], 130.0);
    }

    #[test]
    fn collects_validity_predicate_metrics_rendered_by_the_visualizer() {
        let previous = PrometheusSnapshot::parse(
            r#"# TYPE reth_base_builder_validity_predicate_eval_duration_per_block histogram
reth_base_builder_validity_predicate_eval_duration_per_block_sum 0.2
reth_base_builder_validity_predicate_eval_duration_per_block_count 2
# TYPE reth_base_builder_validity_predicate_evaluations_total counter
reth_base_builder_validity_predicate_evaluations_total{outcome="matched"} 10
reth_base_builder_validity_predicate_evaluations_total{outcome="not_satisfied"} 40
# TYPE reth_base_builder_resting_predicate_parked_total counter
reth_base_builder_resting_predicate_parked_total 100
# TYPE reth_base_builder_resting_predicate_carry_released_total counter
reth_base_builder_resting_predicate_carry_released_total{reason="pre_block_state"} 5
"#,
        );
        let current = PrometheusSnapshot::parse(
            r#"# TYPE reth_base_builder_validity_predicate_eval_duration_per_block histogram
reth_base_builder_validity_predicate_eval_duration_per_block_sum 0.5
reth_base_builder_validity_predicate_eval_duration_per_block_count 4
# TYPE reth_base_builder_validity_predicate_evaluations_total counter
reth_base_builder_validity_predicate_evaluations_total{outcome="matched"} 13
reth_base_builder_validity_predicate_evaluations_total{outcome="not_satisfied"} 60
# TYPE reth_base_builder_resting_predicate_parked_total counter
reth_base_builder_resting_predicate_parked_total 180
# TYPE reth_base_builder_resting_predicate_carry_released_total counter
reth_base_builder_resting_predicate_carry_released_total{reason="pre_block_state"} 7
# TYPE reth_base_builder_resting_predicate_carried histogram
reth_base_builder_resting_predicate_carried_sum 90
reth_base_builder_resting_predicate_carried_count 2
# TYPE reth_base_builder_validity_candidate_handling_duration histogram
reth_base_builder_validity_candidate_handling_duration_sum 0.03
reth_base_builder_validity_candidate_handling_duration_count 10
"#,
        );
        let delta = current.delta(&previous);
        let expected = [
            ("reth_base_builder_validity_predicate_eval_duration_per_block_avg", 0.15),
            ("reth_base_builder_validity_predicate_evaluations_total_outcome_matched", 3.0),
            ("reth_base_builder_validity_predicate_evaluations_total_outcome_not_satisfied", 20.0),
            ("reth_base_builder_resting_predicate_parked_total", 80.0),
            (
                "reth_base_builder_resting_predicate_carry_released_total_reason_pre_block_state",
                2.0,
            ),
            ("reth_base_builder_resting_predicate_carried_avg", 45.0),
            ("reth_base_builder_validity_candidate_handling_duration_avg", 0.003),
        ];
        for (name, value) in expected {
            assert!(
                PrometheusBlockCollector::is_visualizer_metric(name),
                "{name} must be kept for the visualizer"
            );
            assert!(
                (delta[name] - value).abs() < 1e-9,
                "{name}: expected {value}, got {}",
                delta[name]
            );
        }
        assert!(!PrometheusBlockCollector::is_visualizer_metric(
            "reth_base_builder_validity_predicate_evaluations_total"
        ));
    }
}
