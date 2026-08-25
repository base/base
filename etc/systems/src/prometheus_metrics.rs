//! Per-block Prometheus metric collection for benchmarked execution nodes.

use std::{collections::BTreeMap, time::Duration};

use alloy_provider::{Provider, RootProvider};
use base_common_network::Base;
use eyre::{Result, WrapErr};
use tokio::{sync::watch, task::JoinHandle};
use url::Url;

/// Prometheus metric family type relevant to benchmark aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrometheusMetricKind {
    /// Monotonically increasing counter.
    Counter,
    /// Point-in-time gauge.
    Gauge,
    /// Histogram represented by sum and count samples.
    Histogram,
    /// Summary represented by sum and count samples.
    Summary,
    /// Metric without a declared type.
    Untyped,
}

/// Parsed values from one Prometheus endpoint scrape.
#[derive(Debug, Clone, Default)]
pub struct PrometheusSnapshot {
    values: BTreeMap<String, f64>,
    kinds: BTreeMap<String, PrometheusMetricKind>,
}

impl PrometheusSnapshot {
    /// Parses Prometheus text exposition into numeric samples.
    pub fn parse(input: &str) -> Self {
        let mut kinds = BTreeMap::new();
        let mut values = BTreeMap::new();
        for line in input.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let mut fields = rest.split_whitespace();
                if let (Some(name), Some(kind)) = (fields.next(), fields.next()) {
                    kinds.insert(name.to_string(), Self::parse_kind(kind));
                }
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(separator) = line.rfind(char::is_whitespace) else {
                continue;
            };
            let sample = line[..separator].trim();
            let Ok(value) = line[separator..].trim().parse::<f64>() else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            let key = Self::sample_key(sample);
            values.insert(key, value);
        }
        Self { values, kinds }
    }

    /// Produces report-ready gauges and per-scrape counter/histogram deltas.
    pub fn delta(&self, previous: &Self) -> BTreeMap<String, f64> {
        self.delta_for_blocks(previous, 1)
    }

    /// Produces per-block values when one scrape spans multiple canonical blocks.
    pub fn delta_for_blocks(&self, previous: &Self, block_count: u64) -> BTreeMap<String, f64> {
        let divisor = block_count.max(1) as f64;
        let mut metrics = BTreeMap::new();
        for (key, value) in &self.values {
            let family = Self::metric_family(key, &self.kinds);
            match self.kinds.get(family).copied().unwrap_or(PrometheusMetricKind::Untyped) {
                PrometheusMetricKind::Counter => {
                    metrics.insert(
                        key.clone(),
                        Self::counter_delta(*value, previous.values.get(key)) / divisor,
                    );
                }
                PrometheusMetricKind::Gauge | PrometheusMetricKind::Untyped => {
                    metrics.insert(key.clone(), *value);
                }
                PrometheusMetricKind::Histogram | PrometheusMetricKind::Summary => {}
            }
        }

        for (family, kind) in &self.kinds {
            if !matches!(kind, PrometheusMetricKind::Histogram | PrometheusMetricKind::Summary) {
                continue;
            }
            let sum_key = format!("{family}_sum");
            let count_key = format!("{family}_count");
            let (Some(sum), Some(count)) = (self.values.get(&sum_key), self.values.get(&count_key))
            else {
                continue;
            };
            let sum_delta = Self::counter_delta(*sum, previous.values.get(&sum_key));
            let count_delta = Self::counter_delta(*count, previous.values.get(&count_key));
            if count_delta > 0.0 {
                metrics.insert(format!("{family}_avg"), sum_delta / count_delta);
            }
        }
        metrics
    }

    fn parse_kind(kind: &str) -> PrometheusMetricKind {
        match kind {
            "counter" => PrometheusMetricKind::Counter,
            "gauge" => PrometheusMetricKind::Gauge,
            "histogram" => PrometheusMetricKind::Histogram,
            "summary" => PrometheusMetricKind::Summary,
            _ => PrometheusMetricKind::Untyped,
        }
    }

    fn sample_key(sample: &str) -> String {
        let Some((name, labels)) = sample.split_once('{') else {
            return sample.to_string();
        };
        let mut labels = labels
            .trim_end_matches('}')
            .split(',')
            .filter_map(|label| label.split_once('='))
            .filter(|(name, _)| *name != "le" && *name != "quantile")
            .map(|(name, value)| {
                format!("{}_{}", Self::sanitize(name), Self::sanitize(value.trim_matches('"')))
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

    fn metric_family<'a>(
        key: &'a str,
        kinds: &'a BTreeMap<String, PrometheusMetricKind>,
    ) -> &'a str {
        kinds
            .keys()
            .filter(|family| {
                key == family.as_str()
                    || key.strip_prefix(family.as_str()).is_some_and(|suffix| {
                        suffix.starts_with('_')
                            && (key.ends_with("_sum")
                                || key.ends_with("_count")
                                || !kinds.contains_key(key))
                    })
            })
            .max_by_key(|family| family.len())
            .map(String::as_str)
            .unwrap_or(key)
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
    /// Starts polling an execution RPC head and scraping its Prometheus endpoint.
    pub async fn start(rpc_url: Url, metrics_url: Url) -> Result<Self> {
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
                    metrics.retain(|name, _| Self::is_diagnostic_metric(name));
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
                    // production when requested continuously. Canonical block totals remain exact;
                    // diagnostic counters are explicitly attributed across this scrape interval.
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
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

    /// Returns whether a flattened metric belongs to the stable benchmark diagnostic set.
    pub fn is_diagnostic_metric(name: &str) -> bool {
        [
            "reth_base_builder_",
            "reth_sync_execution_",
            "reth_sync_block_validation_",
            "reth_sync_state_provider_",
            "reth_consensus_engine_beacon_block_insert_",
            "reth_consensus_engine_beacon_new_payload_",
            "reth_consensus_engine_persistence_save_blocks_",
            "reth_storage_providers_database_save_blocks_",
            "reth_tree_root_sparse_trie_",
            "reth_parallel_sparse_trie_",
            "reth_trie_proof_task_",
            "reth_trie_cursor_overall_duration",
            "reth_trie_hashed_cursor_overall_duration",
            "reth_trie_leaves_added",
            "reth_trie_branches_added",
            "reth_transaction_pool_pending_pool_transactions",
            "reth_transaction_pool_total_transactions",
            "reth_db_freelist",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::{PrometheusBlockCollector, PrometheusSnapshot};

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
    fn selects_only_report_diagnostic_families() {
        assert!(PrometheusBlockCollector::is_diagnostic_metric(
            "reth_base_builder_total_block_built_duration_avg"
        ));
        assert!(PrometheusBlockCollector::is_diagnostic_metric(
            "reth_sync_execution_execution_duration"
        ));
        assert!(!PrometheusBlockCollector::is_diagnostic_metric(
            "reth_rpc_server_calls_started_total_method_eth_call"
        ));
    }
}
