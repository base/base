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
    /// Statistics over scrape-attributed per-block predicate samples, not individual evaluations.
    pub predicate_summary: BTreeMap<String, PredicateMetricSummary>,
}

/// Statistics over finite per-block metric samples; quantiles use nearest rank.
#[derive(Debug, Serialize)]
pub struct PredicateMetricSummary {
    /// Benchmark total, present only for additive sums and counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Arithmetic mean of the block samples.
    pub mean: f64,
    /// Nearest-rank median of the block samples.
    pub p50: f64,
    /// Nearest-rank 99th percentile of the block samples.
    pub p99: f64,
}

impl PredicateMetricSummary {
    /// Summarizes finite samples; an empty series has zero statistics.
    pub fn from_samples(mut samples: Vec<f64>) -> Self {
        samples.retain(|value| value.is_finite());
        samples.sort_by(f64::total_cmp);
        let total = samples.iter().sum();
        let count = samples.len();
        let percentile = |pct: usize| {
            samples.get((count * pct).div_ceil(100).saturating_sub(1)).copied().unwrap_or_default()
        };
        Self { total: Some(total), mean: if count == 0 { 0.0 } else { total / count as f64 },
            p50: percentile(50), p99: percentile(99) }
    }
}

impl SnapshotBenchmarkResult {
    /// Summarizes predicate diagnostics attributed to the measured canonical block window.
    pub fn summarize_predicates(blocks: &[SnapshotBlockMetrics]) -> BTreeMap<String, PredicateMetricSummary> {
        let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for block in blocks {
            for (name, value) in &block.prometheus_metrics {
                if name.starts_with("reth_base_builder_validity_predicate_")
                    || name == "reth_base_builder_resting_predicates" {
                    series.entry(name.clone()).or_default().push(*value);
                }
            }
        }
        series.into_iter().map(|(name, values)| {
            let mut summary = PredicateMetricSummary::from_samples(values);
            if !name.ends_with("_sum") && !name.ends_with("_count") {
                summary.total = None;
            }
            (name, summary)
        }).collect()
    }
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

    /// Predicate summary statistics retain exact totals and nearest-rank percentiles.
    #[test]
    fn predicate_summary_has_exact_totals_and_percentiles() {
        let samples = [2.0, 4.0, 6.0, 8.0];
        let summary = super::PredicateMetricSummary::from_samples(samples.to_vec());
        assert_eq!(summary.total, Some(20.0), "all four block values contribute to the total");
        assert_eq!(summary.mean, 5.0, "mean uses all four measured blocks");
        assert_eq!(summary.p50, 4.0, "nearest-rank median is the second sample");
        assert_eq!(summary.p99, 8.0, "nearest-rank p99 is the final sample");
        let json = serde_json::to_value(summary).unwrap();
        assert_eq!(json["total"], 20.0, "the summary total reaches result JSON");
    }

    /// Only additive predicate samples have a meaningful benchmark total.
    #[test]
    fn predicate_summary_omits_nonadditive_totals() {
        let names = ["validity_predicate_evaluation_duration_sum", "validity_predicate_evaluation_duration_count",
            "validity_predicate_evaluation_duration_avg", "validity_predicate_evaluations_per_second", "resting_predicates"];
        let blocks = [SnapshotBlockMetrics { number: 1, hash: B256::ZERO, timestamp: 0, timestamp_ms: 0,
            gas_used: 0, gas_limit: 0, transaction_count: 0, prometheus_metrics: names.iter()
                .map(|name| (format!("reth_base_builder_{name}"), 3.0)).collect() }];
        let json = serde_json::to_value(SnapshotBenchmarkResult::summarize_predicates(&blocks)).unwrap();
        for (index, name) in names.iter().enumerate() {
            let summary = &json[format!("reth_base_builder_{name}")];
            assert_eq!(summary.get("total").is_some(), index < 2, "only additive totals: {name}");
            if index < 2 {
                assert_eq!(summary["total"], 3.0, "additive samples retain their exact total: {name}");
            }
            assert_eq!(summary["mean"], 3.0, "every series retains its exact mean: {name}");
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
        let mut result = SnapshotBenchmarkResult {
            chain_id: 8453,
            predicate_summary: BTreeMap::new(),
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
        let name = "reth_base_builder_validity_predicate_evaluations_per_block_sum";
        result.blocks[0].prometheus_metrics.insert(name.to_string(), 12.0);
        result.predicate_summary = SnapshotBenchmarkResult::summarize_predicates(&result.blocks);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["blocks"][0]["prometheus_metrics"][name], 12.0,
            "the exact per-block evaluation count reaches result JSON");
        assert_eq!(json["predicate_summary"][name]["total"], 12.0,
            "the summary retains the exact evaluation total");
        assert_eq!(json["predicate_summary"][name]["p99"], 12.0,
            "a single block has exactly its own value at p99");

        report.write_visualizer_bundle(&result).unwrap();

        let metrics: serde_json::Value = serde_json::from_slice(
            &std::fs::read(output.path().join("metrics-sequencer.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metrics[0]["BlockNumber"], 0.1);
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
            predicate_summary: BTreeMap::new(),
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
