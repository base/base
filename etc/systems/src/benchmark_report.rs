//! Artifact types and JSON writer for snapshot-backed benchmarks.

use std::{collections::BTreeMap, path::PathBuf};

use alloy_primitives::{Address, B256};
use base_load_tests::MetricsSummary;
use chrono::{SecondsFormat, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Machine-readable result from one snapshot benchmark case.
#[derive(Debug, Serialize)]
pub struct SnapshotBenchmarkResult {
    /// L2 chain ID of the continued snapshot.
    pub chain_id: u64,
    /// Selected interval between blocks, in milliseconds.
    pub block_interval_ms: u64,
    /// Immutable snapshot boundary block number.
    pub boundary_number: u64,
    /// Immutable snapshot boundary block hash.
    pub boundary_hash: B256,
    /// Builder RPC used by the load test.
    pub builder_rpc_url: String,
    /// Follow-client RPC used for propagation validation, if a follow client ran.
    pub client_rpc_url: Option<String>,
    /// Ephemeral or explicitly configured load-test funder address.
    pub funder_address: Address,
    /// Native load-tester metrics.
    pub load_test: MetricsSummary,
    /// Canonical block metrics for the complete measured window, including warmup transactions.
    pub blocks: Vec<SnapshotBlockMetrics>,
    /// Follow-client copy of the canonical measured window; empty without a follow client.
    pub validator_blocks: Vec<SnapshotBlockMetrics>,
}

/// Canonical metrics for one block in the measured benchmark window.
#[derive(Debug, Serialize)]
pub struct SnapshotBlockMetrics {
    /// Canonical L2 block number.
    pub number: u64,
    /// Canonical L2 block hash.
    pub hash: B256,
    /// L2 timestamp in Unix seconds.
    pub timestamp: u64,
    /// L2 timestamp in Unix milliseconds, including `BaseTime` precision.
    pub timestamp_ms: u64,
    /// Gas consumed by all transactions in the block.
    pub gas_used: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Number of all transactions in the block.
    pub transaction_count: u64,
    /// Prometheus gauges and per-block counter/histogram deltas.
    pub prometheus_metrics: BTreeMap<String, f64>,
}

/// One per-block sample in the base/benchmark visualizer wire format.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct VisualizerBlockMetrics {
    /// Position in two-second-equivalent blocks.
    pub block_number: f64,
    /// Canonical L2 timestamp in milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// Named numeric metrics rendered by the visualizer.
    pub execution_metrics: BTreeMap<String, f64>,
}

/// Top-level metadata document consumed by the base/benchmark report service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualizerMetadata {
    /// Exactly one completed benchmark run.
    pub runs: Vec<VisualizerRun>,
}

/// Metadata for one visualizer benchmark run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualizerRun {
    /// Unique run ID.
    pub id: String,
    /// Network/source label used by the visualizer.
    pub source_file: String,
    /// Directory containing this run's artifacts.
    pub output_dir: String,
    /// Human-readable benchmark name.
    pub test_name: String,
    /// Human-readable benchmark description.
    pub test_description: String,
    /// Stable filter and comparison dimensions.
    pub test_config: BTreeMap<String, serde_json::Value>,
    /// Completion status and headline metrics.
    pub result: VisualizerRunResult,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

/// Headline metrics and artifacts for a visualizer run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualizerRunResult {
    /// Whether the load test completed without a fatal error.
    pub success: bool,
    /// Whether all bundle files were produced.
    pub complete: bool,
    /// Stable client build identifier.
    pub client_version: String,
    /// Sequencer headline metrics.
    pub sequencer_metrics: VisualizerSequencerMetrics,
    /// Validator headline metrics, if a follow client ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator_metrics: Option<VisualizerValidatorMetrics>,
    /// Additional result artifacts.
    pub artifacts: BTreeMap<String, String>,
}

/// Sequencer summary fields understood by base/benchmark.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualizerSequencerMetrics {
    /// Average measured gas per second.
    pub gas_per_second: f64,
}

/// Validator summary fields understood by base/benchmark.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualizerValidatorMetrics {
    /// Average propagated measured gas per second.
    pub gas_per_second: f64,
}

/// Identifies a completed snapshot benchmark run and its artifact directory.
#[derive(Debug, Clone)]
pub struct SnapshotBenchmarkReportConfig {
    output_dir: PathBuf,
    output_name: String,
    run_id: String,
    benchmark_run: String,
    scenario: String,
    client_version: String,
}

impl SnapshotBenchmarkReportConfig {
    /// Creates a report writer for one completed benchmark run.
    pub const fn new(
        output_dir: PathBuf,
        output_name: String,
        run_id: String,
        benchmark_run: String,
        scenario: String,
        client_version: String,
    ) -> Self {
        Self { output_dir, output_name, run_id, benchmark_run, scenario, client_version }
    }

    /// Writes files directly consumable by the base/benchmark report visualizer.
    pub fn write_visualizer_bundle(&self, result: &SnapshotBenchmarkResult) -> Result<()> {
        if result.blocks.is_empty() {
            warn!(
                output_dir = %self.output_dir.display(),
                sequencer_blocks = result.blocks.len(),
                validator_blocks = result.validator_blocks.len(),
                "skipping visualizer bundle because measured block metrics are empty"
            );
            return Ok(());
        }
        std::fs::create_dir_all(&self.output_dir).wrap_err_with(|| {
            format!("failed to create visualizer output directory {}", self.output_dir.display())
        })?;

        let block_seconds = result.block_interval_ms as f64 / 1_000.0;
        let block_metrics = result
            .blocks
            .iter()
            .enumerate()
            .map(|(index, block)| VisualizerBlockMetrics {
                block_number: Self::normalized_block_number(index, result.block_interval_ms),
                timestamp: block.timestamp_ms,
                execution_metrics: Self::visualizer_metrics(block, block_seconds),
            })
            .collect::<Vec<_>>();
        let validator_metrics = result
            .validator_blocks
            .iter()
            .enumerate()
            .map(|(index, block)| VisualizerBlockMetrics {
                block_number: Self::normalized_block_number(index, result.block_interval_ms),
                timestamp: block.timestamp_ms,
                execution_metrics: Self::visualizer_metrics(block, block_seconds),
            })
            .collect::<Vec<_>>();
        std::fs::write(
            self.output_dir.join("metrics-sequencer.json"),
            serde_json::to_vec_pretty(&block_metrics)?,
        )?;
        if !validator_metrics.is_empty() {
            std::fs::write(
                self.output_dir.join("metrics-validator.json"),
                serde_json::to_vec_pretty(&validator_metrics)?,
            )?;
        }
        std::fs::write(
            self.output_dir.join("load-test-result.json"),
            serde_json::to_vec_pretty(&result.load_test)?,
        )?;

        let sequencer_gas_per_second = result.blocks.iter().map(|block| block.gas_used).sum::<u64>()
            as f64
            / (result.blocks.len() as f64 * block_seconds);
        let validator_gas_per_second =
            result.validator_blocks.iter().map(|block| block.gas_used).sum::<u64>() as f64
                / (result.validator_blocks.len() as f64 * block_seconds);
        let transaction_payload = Self::transaction_payload(result);
        let test_config = BTreeMap::from([
            ("BenchmarkRun".to_string(), serde_json::Value::String(self.benchmark_run.clone())),
            ("Scenario".to_string(), serde_json::Value::String(self.scenario.clone())),
            ("ChainId".to_string(), result.chain_id.into()),
            ("BlockTimeMilliseconds".to_string(), result.block_interval_ms.into()),
            ("NodeType".to_string(), serde_json::Value::String("base-reth-node".to_string())),
            ("TransactionPayload".to_string(), serde_json::Value::String(transaction_payload)),
            ("ClientVersion".to_string(), serde_json::Value::String(self.client_version.clone())),
        ]);
        let metadata = VisualizerMetadata {
            runs: vec![VisualizerRun {
                id: self.run_id.clone(),
                source_file: format!("base-{}-snapshot", result.chain_id),
                output_dir: self.output_name.clone(),
                test_name: format!("Base {} snapshot throughput", result.chain_id),
                test_description: format!(
                    "Saturated block production from a Base {} snapshot",
                    result.chain_id
                ),
                test_config,
                result: VisualizerRunResult {
                    success: result.load_test.error.is_none(),
                    complete: true,
                    client_version: self.client_version.clone(),
                    sequencer_metrics: VisualizerSequencerMetrics {
                        gas_per_second: sequencer_gas_per_second,
                    },
                    validator_metrics: (!validator_metrics.is_empty()).then_some(
                        VisualizerValidatorMetrics { gas_per_second: validator_gas_per_second },
                    ),
                    artifacts: BTreeMap::from([(
                        "loadTestResult".to_string(),
                        "load-test-result.json".to_string(),
                    )]),
                },
                created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            }],
        };
        std::fs::write(
            self.output_dir.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata)?,
        )?;
        Ok(())
    }

    /// Adds window quantiles from summed bucket deltas, never from per-scrape percentiles.
    /// Raw buckets are divided across blocks by the collector, so skipped heads are counted
    /// once. Partial edge scrape intervals are proportionally attributed, like existing counters.
    /// Gauge averages weight each scrape once, rather than once per covered block.
    pub fn add_window_metrics(blocks: &mut [SnapshotBlockMetrics]) {
        let mut histograms: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
        let mut gauges = Vec::new();
        for block in &*blocks {
            let metrics = &block.prometheus_metrics;
            for (key, count) in metrics {
                let Some(bucket) = key.strip_prefix("benchmark/histogram/") else { continue; };
                let Some((name, bound)) = bucket.rsplit_once('/') else { continue; };
                let Ok(bound) = bound.parse::<f64>() else { continue; };
                let buckets = histograms.entry(name.to_owned()).or_default();
                if let Some((_, value)) = buckets.iter_mut().find(|(edge, _)| *edge == bound) {
                    *value += count;
                } else {
                    buckets.push((bound, *count));
                }
            }
            if let Some(value) = metrics.get(crate::prometheus_metrics::WINDOW_GAUGE) {
                let weight = 1.0 / metrics.get("benchmark/prometheus_blocks_per_scrape").copied().unwrap_or(1.0).max(1.0);
                gauges.push((*value, weight));
            }
        }
        let mut window = BTreeMap::new();
        for (name, mut buckets) in histograms {
            buckets.sort_by(|left, right| left.0.total_cmp(&right.0));
            for (suffix, quantile) in [("p50", 0.5), ("p95", 0.95), ("p99", 0.99)] {
                if let Some(value) = crate::PrometheusSnapshot::histogram_quantile(&buckets, quantile) {
                    window.insert(format!("{name}_window_{suffix}"), value);
                }
            }
        }
        if !gauges.is_empty() {
            let name = crate::prometheus_metrics::WINDOW_GAUGE;
            window.insert(format!("{name}_sampled_min"), gauges.iter().map(|(v, _)| *v).fold(f64::INFINITY, f64::min));
            window.insert(format!("{name}_sampled_max"), gauges.iter().map(|(v, _)| *v).fold(f64::NEG_INFINITY, f64::max));
            window.insert(format!("{name}_sampled_avg"), gauges.iter().map(|(v, w)| v * w).sum::<f64>() / gauges.iter().map(|(_, w)| w).sum::<f64>());
        }
        for block in blocks {
            block.prometheus_metrics.retain(|name, _| !name.starts_with("benchmark/histogram/"));
            block.prometheus_metrics.extend(window.clone());
        }
    }

    /// Converts a zero-based sample index to the equivalent position on a two-second block axis.
    pub fn normalized_block_number(index: usize, block_interval_ms: u64) -> f64 {
        const REFERENCE_BLOCK_INTERVAL_MS: f64 = 2_000.0;
        (index as f64 + 1.0) * block_interval_ms as f64 / REFERENCE_BLOCK_INTERVAL_MS
    }

    /// Combines canonical block totals with metrics scraped while that block was produced.
    fn visualizer_metrics(
        block: &SnapshotBlockMetrics,
        block_seconds: f64,
    ) -> BTreeMap<String, f64> {
        Self::block_visualizer_metrics(
            block.prometheus_metrics.clone(),
            block.gas_used,
            block.transaction_count,
            block_seconds,
        )
    }

    /// Adds a block's gas and transaction totals to its scraped metrics, in the visualizer's keys.
    pub fn block_visualizer_metrics(
        mut metrics: BTreeMap<String, f64>,
        gas_used: u64,
        transaction_count: u64,
        block_seconds: f64,
    ) -> BTreeMap<String, f64> {
        metrics.insert("gas/per_block".to_string(), gas_used as f64);
        metrics.insert("gas/per_second".to_string(), gas_used as f64 / block_seconds);
        metrics.insert("transactions/per_block".to_string(), transaction_count as f64);
        metrics.insert(
            "transactions/per_second".to_string(),
            transaction_count as f64 / block_seconds,
        );
        metrics
    }

    /// Returns a stable report filter value for the configured workload.
    fn transaction_payload(result: &SnapshotBenchmarkResult) -> String {
        let Some(config) = result.load_test.config.as_ref() else {
            return "unknown".to_string();
        };
        if let Some(transactions) = config.transactions.as_array()
            && transactions.len() == 1
            && let Some(transaction) = transactions.first().and_then(serde_json::Value::as_object)
        {
            let transaction_type = transaction.get("type").and_then(serde_json::Value::as_str);
            if transaction_type == Some("precompile")
                && let Some(target) = transaction.get("target").and_then(serde_json::Value::as_str)
            {
                return target.to_string();
            }
            if let Some(transaction_type) = transaction_type
                && transaction_type != "transfer"
            {
                return transaction_type.to_string();
            }
        }
        if config.fresh_recipient_ratio >= 1.0 {
            "fresh-account".to_string()
        } else if config.fresh_recipient_ratio <= 0.0 {
            "existing-account".to_string()
        } else {
            "mixed-account".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{Address, B256};
    use base_load_tests::{MetricsSummary, TestConfig, ThroughputMetrics};

    use super::{SnapshotBenchmarkReportConfig, SnapshotBenchmarkResult, SnapshotBlockMetrics};

    /// Pool unequal scrape intervals once and retain existing per-block averages.
    #[test]
    fn window_metrics_pool_buckets_and_weight_scrapes_once() {
        let sample = |number, counts: [f64; 3], gauge| {
            let mut metrics = BTreeMap::from([
                (crate::WINDOW_GAUGE.to_owned(), gauge),
                ("benchmark/prometheus_blocks_per_scrape".to_owned(), if number == 1 { 1.0 } else { 2.0 }),
                ("build_avg".to_owned(), 0.25),
            ]);
            for (bound, count) in ["1", "2", "inf"].into_iter().zip(counts) {
                metrics.insert(format!("benchmark/histogram/build/{bound}"), count);
            }
            SnapshotBlockMetrics {
                number, hash: B256::ZERO, timestamp: 0, timestamp_ms: 0,
                gas_used: 0, gas_limit: 1, transaction_count: 0, prometheus_metrics: metrics,
            }
        };
        let mut blocks = vec![sample(1, [1.0, 1.0, 1.0], 2.0),
            sample(2, [0.0, 1.0, 1.0], 8.0), sample(3, [0.0, 1.0, 1.0], 8.0)];
        SnapshotBenchmarkReportConfig::add_window_metrics(&mut blocks);
        for block in &blocks {
            let metrics = &block.prometheus_metrics;
            assert_eq!(metrics["build_window_p50"], 1.25, "pooled median must not average scrape percentiles");
            assert_eq!(metrics["build_avg"], 0.25, "existing per-block averages remain untouched");
            assert_eq!(metrics[&format!("{}_sampled_avg", crate::WINDOW_GAUGE)], 5.0, "two-block scrape is counted once");
            assert_eq!(metrics[&format!("{}_sampled_min", crate::WINDOW_GAUGE)], 2.0, "sampled minimum is exact");
            assert_eq!(metrics[&format!("{}_sampled_max", crate::WINDOW_GAUGE)], 8.0, "sampled maximum is exact");
            assert!(!metrics.keys().any(|key| key.starts_with("benchmark/histogram/")), "internal bucket keys are removed from reports");
        }
    }

    #[test]
    fn writes_visualizer_data_contract() {
        let output = tempfile::tempdir().unwrap();
        let output_name = output.path().file_name().unwrap().to_string_lossy().to_string();
        let report = SnapshotBenchmarkReportConfig::new(
            output.path().to_path_buf(),
            output_name.clone(),
            "run-123".to_string(),
            "snapshot-throughput".to_string(),
            "blake2f-200ms".to_string(),
            "base/v0.0.0-test123".to_string(),
        );
        let result = SnapshotBenchmarkResult {
            chain_id: 8453,
            block_interval_ms: 200,
            boundary_number: 9,
            boundary_hash: B256::ZERO,
            builder_rpc_url: "http://127.0.0.1:1".to_string(),
            client_rpc_url: Some("http://127.0.0.1:2".to_string()),
            funder_address: Address::ZERO,
            load_test: MetricsSummary {
                throughput: ThroughputMetrics { gps: 2_000_000_000.0, ..Default::default() },
                config: Some(
                    TestConfig::from_yaml(
                        r#"
transaction_submission_rpcs: http://127.0.0.1:1
transactions:
  - weight: 100
    type: precompile
    target: blake2f
    rounds: 50000
"#,
                    )
                    .unwrap()
                    .to_summary(),
                ),
                ..Default::default()
            },
            blocks: vec![SnapshotBlockMetrics {
                number: 10,
                hash: B256::ZERO,
                timestamp: 1,
                timestamp_ms: 1_000,
                gas_used: 400_000_000,
                gas_limit: 400_000_000,
                transaction_count: 19_047,
                prometheus_metrics: BTreeMap::from([(
                    "reth_base_builder_total_block_built_duration_avg".to_string(),
                    0.1,
                )]),
            }],
            validator_blocks: vec![SnapshotBlockMetrics {
                number: 10,
                hash: B256::ZERO,
                timestamp: 1,
                timestamp_ms: 1_000,
                gas_used: 400_000_000,
                gas_limit: 400_000_000,
                transaction_count: 19_047,
                prometheus_metrics: BTreeMap::new(),
            }],
        };

        let mut result = result;
        for (bound, count) in [("1", 2.0), ("2", 4.0), ("inf", 4.0)] {
            result.blocks[0].prometheus_metrics.insert(
                format!("benchmark/histogram/reth_base_builder_native_payload_build_duration/{bound}"), count,
            );
        }
        result.blocks[0].prometheus_metrics.insert(
            crate::prometheus_metrics::WINDOW_GAUGE.to_owned(), 12.0,
        );
        SnapshotBenchmarkReportConfig::add_window_metrics(&mut result.blocks);
        report.write_visualizer_bundle(&result).unwrap();

        let metrics: serde_json::Value = serde_json::from_slice(
            &std::fs::read(output.path().join("metrics-sequencer.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metrics[0]["BlockNumber"], 0.1);
        assert_eq!(metrics[0]["ExecutionMetrics"]["reth_base_builder_native_payload_build_duration_window_p99"], 1.98, "JSON report must include the bucket-derived window p99");
        assert_eq!(metrics[0]["ExecutionMetrics"]["reth_base_builder_native_payload_build_duration_window_p50"], 1.0, "JSON contains window median");
        assert_eq!(metrics[0]["ExecutionMetrics"]["reth_base_builder_native_payload_build_duration_window_p95"], 1.9, "JSON contains window p95");
        for suffix in ["min", "avg", "max"] {
            assert_eq!(metrics[0]["ExecutionMetrics"][format!("{}_sampled_{suffix}", crate::prometheus_metrics::WINDOW_GAUGE)], 12.0, "single sampled gauge window has exact min/avg/max");
        }
        assert_eq!(metrics[0]["Timestamp"], 1_000);
        assert_eq!(metrics[0]["ExecutionMetrics"]["gas/per_block"], 400_000_000.0);
        assert_eq!(metrics[0]["ExecutionMetrics"]["gas/per_second"], 2_000_000_000.0);
        assert_eq!(metrics[0]["ExecutionMetrics"]["transactions/per_second"], 95_235.0);
        assert_eq!(
            metrics[0]["ExecutionMetrics"]["reth_base_builder_total_block_built_duration_avg"],
            0.1
        );

        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.path().join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["runs"][0]["result"]["success"], true);
        assert_eq!(metadata["runs"][0]["id"], "run-123");
        assert_eq!(metadata["runs"][0]["outputDir"], output_name);
        assert_eq!(metadata["runs"][0]["testConfig"]["BenchmarkRun"], "snapshot-throughput");
        assert_eq!(metadata["runs"][0]["testConfig"]["Scenario"], "blake2f-200ms");
        assert!(metadata["runs"][0]["testConfig"].get("GasLimit").is_none());
        assert_eq!(metadata["runs"][0]["testConfig"]["TransactionPayload"], "blake2f");
        assert_eq!(
            metadata["runs"][0]["result"]["artifacts"]["loadTestResult"],
            "load-test-result.json"
        );
        assert!(output.path().join("metrics-validator.json").is_file());
        assert!(output.path().join("load-test-result.json").is_file());
    }

    #[test]
    fn builder_only_bundle_omits_validator_output() {
        let output = tempfile::tempdir().unwrap();
        let report = SnapshotBenchmarkReportConfig::new(
            output.path().to_path_buf(),
            "builder-only".to_string(),
            "run-123".to_string(),
            "snapshot-throughput".to_string(),
            "transfer-2s".to_string(),
            "base/v0.0.0-test123".to_string(),
        );
        let result = SnapshotBenchmarkResult {
            chain_id: 8453,
            block_interval_ms: 2_000,
            boundary_number: 9,
            boundary_hash: B256::ZERO,
            builder_rpc_url: "http://127.0.0.1:1".to_string(),
            client_rpc_url: None,
            funder_address: Address::ZERO,
            load_test: MetricsSummary::default(),
            blocks: vec![SnapshotBlockMetrics {
                number: 10,
                hash: B256::ZERO,
                timestamp: 1,
                timestamp_ms: 1_000,
                gas_used: 21_000,
                gas_limit: 400_000_000,
                transaction_count: 1,
                prometheus_metrics: BTreeMap::new(),
            }],
            validator_blocks: Vec::new(),
        };

        report.write_visualizer_bundle(&result).unwrap();

        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.path().join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["runs"][0]["result"]["sequencerMetrics"]["gasPerSecond"], 10_500.0);
        assert!(metadata["runs"][0]["result"].get("validatorMetrics").is_none());
        assert!(output.path().join("metrics-sequencer.json").is_file());
        assert!(!output.path().join("metrics-validator.json").exists());
    }

    #[test]
    fn normalizes_block_numbers_to_two_second_equivalents() {
        assert_eq!(SnapshotBenchmarkReportConfig::normalized_block_number(0, 2_000), 1.0);
        assert_eq!(SnapshotBenchmarkReportConfig::normalized_block_number(1, 2_000), 2.0);
        assert_eq!(SnapshotBenchmarkReportConfig::normalized_block_number(0, 200), 0.1);
        assert_eq!(SnapshotBenchmarkReportConfig::normalized_block_number(9, 200), 1.0);
        assert_eq!(SnapshotBenchmarkReportConfig::normalized_block_number(5_999, 200), 600.0);
    }
}
