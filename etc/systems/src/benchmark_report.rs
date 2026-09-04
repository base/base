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
    /// Follow-client RPC used for propagation validation.
    pub client_rpc_url: String,
    /// Ephemeral or explicitly configured load-test funder address.
    pub funder_address: Address,
    /// Native load-tester metrics.
    pub load_test: MetricsSummary,
    /// Canonical block metrics for the complete measured window, including warmup transactions.
    pub blocks: Vec<SnapshotBlockMetrics>,
    /// Follow-client copy of the canonical measured window.
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
    /// Validator headline metrics.
    pub validator_metrics: VisualizerValidatorMetrics,
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
        if result.blocks.is_empty() || result.validator_blocks.is_empty() {
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
        std::fs::write(
            self.output_dir.join("metrics-validator.json"),
            serde_json::to_vec_pretty(&validator_metrics)?,
        )?;
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
        let gas_limit = result.blocks.first().map(|block| block.gas_limit).unwrap_or_default();
        let transaction_payload = Self::transaction_payload(result);
        let test_config = BTreeMap::from([
            ("BenchmarkRun".to_string(), serde_json::Value::String(self.benchmark_run.clone())),
            ("Scenario".to_string(), serde_json::Value::String(self.scenario.clone())),
            ("ChainId".to_string(), result.chain_id.into()),
            ("BlockTimeMilliseconds".to_string(), result.block_interval_ms.into()),
            ("GasLimit".to_string(), gas_limit.into()),
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
                    validator_metrics: VisualizerValidatorMetrics {
                        gas_per_second: validator_gas_per_second,
                    },
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
    fn normalized_block_number(index: usize, block_interval_ms: u64) -> f64 {
        const REFERENCE_BLOCK_INTERVAL_MS: f64 = 2_000.0;
        (index as f64 + 1.0) * block_interval_ms as f64 / REFERENCE_BLOCK_INTERVAL_MS
    }

    /// Combines canonical block totals with metrics scraped while that block was produced.
    fn visualizer_metrics(
        block: &SnapshotBlockMetrics,
        block_seconds: f64,
    ) -> BTreeMap<String, f64> {
        let mut metrics = block.prometheus_metrics.clone();
        metrics.insert("gas/per_block".to_string(), block.gas_used as f64);
        metrics.insert("gas/per_second".to_string(), block.gas_used as f64 / block_seconds);
        metrics.insert("transactions/per_block".to_string(), block.transaction_count as f64);
        metrics.insert(
            "transactions/per_second".to_string(),
            block.transaction_count as f64 / block_seconds,
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
            client_rpc_url: "http://127.0.0.1:2".to_string(),
            funder_address: Address::ZERO,
            load_test: MetricsSummary {
                throughput: ThroughputMetrics { gps: 2_000_000_000.0, ..Default::default() },
                config: Some(
                    TestConfig::from_yaml(
                        r#"
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
        assert_eq!(metadata["runs"][0]["testConfig"]["GasLimit"], 400_000_000);
        assert_eq!(metadata["runs"][0]["testConfig"]["TransactionPayload"], "blake2f");
        assert_eq!(
            metadata["runs"][0]["result"]["artifacts"]["loadTestResult"],
            "load-test-result.json"
        );
        assert!(output.path().join("metrics-validator.json").is_file());
        assert!(output.path().join("load-test-result.json").is_file());
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
