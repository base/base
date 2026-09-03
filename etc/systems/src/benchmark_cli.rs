//! Command-line orchestration for snapshot-backed benchmarks.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_load_tests::{
    BaselineError, LoadTestDisplay, LoadTestExecutor, LoadTestRunHooks, LoadTestRunOptions,
    MetricsSummary, TestConfig,
};
use chrono::{SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};
use serde::Serialize;
use tracing::warn;

use crate::{
    DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund, PrometheusBlockCollector,
    SnapshotChainConfig, SnapshotL2Stack, SystemTestStackBuilder,
};

/// Base benchmark launcher.
#[derive(Debug, Parser)]
#[command(author, version, about = "Benchmark a Base development network")]
pub struct BenchmarkCli {
    /// Benchmark target.
    #[command(subcommand)]
    pub command: BenchmarkCommand,
}

/// Supported benchmark targets.
#[derive(Debug, Subcommand)]
pub enum BenchmarkCommand {
    /// Run one load test against a Base snapshot continuation.
    Snapshot(SnapshotBenchmarkArgs),
}

/// Arguments for one snapshot-backed benchmark case.
#[derive(Debug, Args)]
pub struct SnapshotBenchmarkArgs {
    /// Built-in Base chain name or path to a Base genesis JSON file.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,
    /// Rollup config JSON for a custom chain JSON whose chain ID is not built in.
    #[arg(long)]
    pub rollup_config: Option<PathBuf>,
    /// Writable builder snapshot datadir.
    #[arg(long, env = "BASE_SNAPSHOT_BUILDER_DATADIR")]
    pub builder_datadir: PathBuf,
    /// Writable client snapshot datadir for the same chain.
    #[arg(long, env = "BASE_SNAPSHOT_CLIENT_DATADIR")]
    pub client_datadir: PathBuf,
    /// Load-test YAML. RPC and Flashblocks URLs are replaced with launched endpoints.
    #[arg(long)]
    pub load_test_config: PathBuf,
    /// Required benchmark artifact directory.
    ///
    /// Writes `<output-dir>/{benchmark-result.json,metadata.json,metrics-sequencer.json,metrics-validator.json,load-test-result.json}`.
    #[arg(long)]
    pub output_dir: PathBuf,
    /// User-visible scenario identifier for the report series.
    #[arg(long)]
    pub scenario: String,
    /// Cohort key shared by runs that should be compared in the report.
    #[arg(long, default_value = "snapshot-throughput")]
    pub benchmark_run: String,
    /// Unique run identifier; defaults to `<benchmark-run>-<timestamp>`.
    #[arg(long)]
    pub run_id: Option<String>,
    /// Stable build identifier for visualizer comparisons.
    #[arg(long, env = "BASE_BENCH_CLIENT_VERSION")]
    pub client_version: Option<String>,
}

/// Wei minted to the benchmark's ephemeral funder in the first local descendant (1000 ETH).
const PREFUND_AMOUNT_WEI: u128 = 1_000_000_000_000_000_000_000;
const RESULT_FILE_NAME: &str = "benchmark-result.json";

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
    ///
    /// A 2s block advances this by `1.0`; a 200ms block advances it by `0.1`, so equal-duration
    /// cadence runs share the same report x-axis.
    pub block_number: f64,
    /// Canonical L2 timestamp in milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// Named numeric metrics rendered by the visualizer.
    pub execution_metrics: BTreeMap<String, f64>,
}

/// Top-level metadata document consumed by the base/benchmark report service.
#[derive(Debug, Serialize)]
pub struct VisualizerMetadata {
    /// Exactly one completed benchmark run.
    pub runs: Vec<VisualizerRun>,
}

/// Metadata for one visualizer benchmark run.
#[derive(Debug, Serialize)]
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
    pub test_config: BTreeMap<&'static str, serde_json::Value>,
    /// Completion status and headline metrics.
    pub result: VisualizerRunResult,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

/// Headline metrics and artifacts for a visualizer run.
#[derive(Debug, Serialize)]
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
    pub artifacts: BTreeMap<&'static str, &'static str>,
}

/// Sequencer summary fields understood by base/benchmark.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualizerSequencerMetrics {
    /// Average measured gas per second.
    pub gas_per_second: f64,
}

/// Validator summary fields understood by base/benchmark.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualizerValidatorMetrics {
    /// Average propagated measured gas per second.
    pub gas_per_second: f64,
}

impl BenchmarkCli {
    /// Runs the selected benchmark case.
    pub async fn run(self) -> Result<()> {
        let _progress = LoadTestDisplay::init_tracing();
        match self.command {
            BenchmarkCommand::Snapshot(args) => args.run().await,
        }
    }
}

impl SnapshotBenchmarkArgs {
    /// Starts the devnet, runs the load test, writes results, and shuts down.
    pub async fn run(self) -> Result<()> {
        let test_config = TestConfig::load(&self.load_test_config)
            .wrap_err("failed to load benchmark load-test configuration")?;
        let block_interval = Self::block_interval_from_test_config(&test_config)?;
        let funder_key = PrivateKeySigner::random();
        let mut devnet = DevnetConfig::snapshot(
            self.builder_datadir.clone(),
            self.client_datadir.clone(),
            SnapshotChainConfig {
                chain: self.chain.clone(),
                rollup_config: self.rollup_config.clone(),
            },
        )?;
        let DevnetL2State::Snapshot(snapshot) = &mut devnet.l2_state else {
            unreachable!("snapshot constructor must create snapshot state")
        };
        snapshot.block_interval = block_interval;
        snapshot.prefund =
            Some(DevnetPrefund { address: funder_key.address(), amount: PREFUND_AMOUNT_WEI });

        std::fs::create_dir_all(&self.output_dir).wrap_err_with(|| {
            format!("failed to create benchmark output directory {}", self.output_dir.display())
        })?;
        let result_path = self.output_dir.join(RESULT_FILE_NAME);
        let visualizer_output_dir = self.output_dir.clone();
        let visualizer_output_name = Self::directory_name_or_default(&self.output_dir);
        let run_id =
            self.run_id.clone().unwrap_or_else(|| Self::derived_run_id(&self.benchmark_run));

        let mut stack = SystemTestStackBuilder::new()
            .with_devnet_config(devnet)
            .build_snapshot_sequencer()
            .await?;
        let benchmark_result =
            self.execute(&mut stack, test_config, block_interval, funder_key).await;
        let shutdown_result = stack.shutdown().await;
        let (result, run_error) = benchmark_result?;
        let encoded = serde_json::to_vec_pretty(&result)?;
        std::fs::write(&result_path, encoded).wrap_err_with(|| {
            format!("failed to write benchmark result {}", result_path.display())
        })?;
        if let Some(error) = run_error {
            return Err(error.into());
        }
        if result.load_test.throughput.total_confirmed == 0 {
            eyre::bail!("benchmark completed without confirmed transactions")
        }
        if result.load_test.gas.total_gas == 0 {
            eyre::bail!("benchmark completed without measured gas")
        }
        if let Some(expected_blocks) =
            result.load_test.config.as_ref().and_then(|config| config.measurement_blocks)
        {
            eyre::ensure!(
                result.load_test.measurement_block_count == expected_blocks,
                "benchmark observed {} of {expected_blocks} requested blocks",
                result.load_test.measurement_block_count
            );
            eyre::ensure!(
                result.blocks.len() as u64 == expected_blocks
                    && result.validator_blocks.len() as u64 == expected_blocks,
                "benchmark block metrics do not contain exactly {expected_blocks} blocks"
            );
        }
        self.write_visualizer_bundle(
            &visualizer_output_dir,
            &visualizer_output_name,
            &run_id,
            &result,
        )?;
        shutdown_result?;
        println!("benchmark result: {}", result_path.display());
        Ok(())
    }

    /// Executes the load test against a running snapshot stack.
    pub async fn execute(
        &self,
        stack: &mut SnapshotL2Stack,
        mut test_config: TestConfig,
        block_interval: DevnetBlockInterval,
        funder_key: PrivateKeySigner,
    ) -> Result<(SnapshotBenchmarkResult, Option<BaselineError>)> {
        let builder_rpc = stack.builder_rpc_url()?;
        test_config.transaction_submission_rpcs = vec![builder_rpc.clone()];
        test_config.query_rpc = Some(builder_rpc.clone());
        test_config.txpool_nodes.clear();
        test_config.flashblocks_ws = (block_interval == DevnetBlockInterval::TwoSeconds)
            .then(|| stack.builder_flashblocks_url())
            .transpose()?;
        test_config.chain_id = Some(stack.chain_id());
        let load_config = test_config.to_load_config(None)?;
        let funder_address = funder_key.address();
        let sequencer_metrics =
            PrometheusBlockCollector::start(builder_rpc.clone(), stack.builder_metrics_url()?)
                .await?;
        let load_result = LoadTestExecutor::run_prepared(
            test_config,
            load_config,
            funder_key,
            LoadTestRunOptions {
                continuous: false,
                install_signal_handler: true,
                skip_drain: true,
            },
            LoadTestRunHooks {
                display: None,
                before_cleanup: (|_: &MetricsSummary| {}) as fn(&MetricsSummary),
            },
        )
        .await;
        let output = load_result?;
        let summary = output.summary;
        let measurement_end = summary
            .measurement_end_block
            .ok_or_else(|| eyre::eyre!("load test did not report a measurement end block"))?;
        let sequencer_metrics = sequencer_metrics.finish(measurement_end).await?;
        stack.stop_sequencer().await?;
        let client_rpc = stack.client_rpc_url()?;
        let validator_metrics =
            PrometheusBlockCollector::start(client_rpc.clone(), stack.client_metrics_url()?)
                .await?;
        stack.start_validator().await?;
        let validator_metrics = validator_metrics.finish(measurement_end).await?;
        let (blocks, validator_blocks) = tokio::try_join!(
            Self::collect_block_metrics(&builder_rpc, &summary, &sequencer_metrics),
            Self::collect_block_metrics(&client_rpc, &summary, &validator_metrics),
        )?;
        eyre::ensure!(
            blocks.len() == validator_blocks.len()
                && blocks
                    .iter()
                    .zip(&validator_blocks)
                    .all(|(builder, validator)| builder.number == validator.number
                        && builder.hash == validator.hash),
            "follow client does not match the builder over the measured block window"
        );

        let result = SnapshotBenchmarkResult {
            chain_id: stack.chain_id(),
            block_interval_ms: block_interval.duration().as_millis() as u64,
            boundary_number: stack.boundary().head.number,
            boundary_hash: stack.boundary().head.hash,
            builder_rpc_url: builder_rpc.to_string(),
            client_rpc_url: client_rpc.to_string(),
            funder_address,
            load_test: summary,
            blocks,
            validator_blocks,
        };
        Ok((result, output.run_error))
    }

    /// Fetches every canonical block in the measured window from the builder.
    pub async fn collect_block_metrics(
        rpc_url: &url::Url,
        summary: &MetricsSummary,
        prometheus_metrics: &BTreeMap<u64, BTreeMap<String, f64>>,
    ) -> Result<Vec<SnapshotBlockMetrics>> {
        let (Some(start), Some(end)) =
            (summary.measurement_start_block, summary.measurement_end_block)
        else {
            return Ok(Vec::new());
        };
        eyre::ensure!(
            end.checked_sub(start) == Some(summary.measurement_block_count),
            "measured block bounds {start}..={end} do not match count {}",
            summary.measurement_block_count
        );
        let provider = RootProvider::<Base>::new_http(rpc_url.clone());
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if provider.get_block_by_number(BlockNumberOrTag::Number(end)).await?.is_some() {
                    return Ok::<_, eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for measured block {end} at {rpc_url}"))??;
        let mut blocks = Vec::with_capacity(summary.measurement_block_count as usize);
        for number in start.saturating_add(1)..=end {
            let block = provider
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .full()
                .await
                .wrap_err_with(|| {
                    format!("failed to fetch measured block {number} from {rpc_url}")
                })?
                .ok_or_else(|| {
                    eyre::eyre!("measured block {number} is unavailable from {rpc_url}")
                })?;
            let prometheus_metrics = prometheus_metrics.get(&number).cloned().ok_or_else(|| {
                eyre::eyre!("Prometheus sample for block {number} from {rpc_url} is missing")
            })?;
            blocks.push(SnapshotBlockMetrics {
                number: block.header.number,
                hash: block.header.hash,
                timestamp: block.header.timestamp,
                timestamp_ms: block
                    .header
                    .timestamp_ms
                    .unwrap_or_else(|| block.header.timestamp.saturating_mul(1_000)),
                gas_used: block.header.gas_used,
                gas_limit: block.header.gas_limit,
                transaction_count: block.transactions.len() as u64,
                prometheus_metrics,
            });
        }
        eyre::ensure!(
            blocks.len() as u64 == summary.measurement_block_count,
            "fetched {} of {} measured blocks",
            blocks.len(),
            summary.measurement_block_count
        );
        Ok(blocks)
    }

    /// Writes files directly consumable by the base/benchmark report visualizer.
    pub fn write_visualizer_bundle(
        &self,
        output_dir: &Path,
        output_name: &str,
        run_id: &str,
        result: &SnapshotBenchmarkResult,
    ) -> Result<()> {
        if result.blocks.is_empty() || result.validator_blocks.is_empty() {
            warn!(
                output_dir = %output_dir.display(),
                sequencer_blocks = result.blocks.len(),
                validator_blocks = result.validator_blocks.len(),
                "skipping visualizer bundle because measured block metrics are empty"
            );
            return Ok(());
        }
        std::fs::create_dir_all(output_dir).wrap_err_with(|| {
            format!("failed to create visualizer output directory {}", output_dir.display())
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
            output_dir.join("metrics-sequencer.json"),
            serde_json::to_vec_pretty(&block_metrics)?,
        )?;
        std::fs::write(
            output_dir.join("metrics-validator.json"),
            serde_json::to_vec_pretty(&validator_metrics)?,
        )?;
        std::fs::write(
            output_dir.join("load-test-result.json"),
            serde_json::to_vec_pretty(&result.load_test)?,
        )?;

        let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let client_version = self.client_version.clone().ok_or_else(|| {
            eyre::eyre!(
                "--client-version or BASE_BENCH_CLIENT_VERSION is required for visualizer output"
            )
        })?;
        let sequencer_gas_per_second = result.blocks.iter().map(|block| block.gas_used).sum::<u64>()
            as f64
            / (result.blocks.len() as f64 * block_seconds);
        let validator_gas_per_second =
            result.validator_blocks.iter().map(|block| block.gas_used).sum::<u64>() as f64
                / (result.validator_blocks.len() as f64 * block_seconds);
        let gas_limit = result.blocks.first().map(|block| block.gas_limit).unwrap_or_default();
        let transaction_payload = Self::transaction_payload(result);
        let test_config = BTreeMap::from([
            ("BenchmarkRun", serde_json::Value::String(self.benchmark_run.clone())),
            ("Scenario", serde_json::Value::String(self.scenario.clone())),
            ("ChainId", result.chain_id.into()),
            ("BlockTimeMilliseconds", result.block_interval_ms.into()),
            ("GasLimit", gas_limit.into()),
            ("NodeType", serde_json::Value::String("base-reth-node".to_string())),
            ("TransactionPayload", serde_json::Value::String(transaction_payload)),
            ("ClientVersion", serde_json::Value::String(client_version.clone())),
        ]);
        let metadata = VisualizerMetadata {
            runs: vec![VisualizerRun {
                id: run_id.to_string(),
                source_file: format!("base-{}-snapshot", result.chain_id),
                output_dir: output_name.to_string(),
                test_name: format!("Base {} snapshot throughput", result.chain_id),
                test_description: format!(
                    "Saturated block production from a Base {} snapshot",
                    result.chain_id
                ),
                test_config,
                result: VisualizerRunResult {
                    success: result.load_test.error.is_none(),
                    complete: true,
                    client_version,
                    sequencer_metrics: VisualizerSequencerMetrics {
                        gas_per_second: sequencer_gas_per_second,
                    },
                    validator_metrics: VisualizerValidatorMetrics {
                        gas_per_second: validator_gas_per_second,
                    },
                    artifacts: BTreeMap::from([("loadTestResult", "load-test-result.json")]),
                },
                created_at,
            }],
        };
        std::fs::write(output_dir.join("metadata.json"), serde_json::to_vec_pretty(&metadata)?)?;
        Ok(())
    }

    /// Converts a zero-based sample index to the equivalent position on a two-second block axis.
    fn normalized_block_number(index: usize, block_interval_ms: u64) -> f64 {
        const REFERENCE_BLOCK_INTERVAL_MS: f64 = 2_000.0;
        (index as f64 + 1.0) * block_interval_ms as f64 / REFERENCE_BLOCK_INTERVAL_MS
    }

    /// Combines canonical block totals with metrics scraped while that block was produced.
    pub fn visualizer_metrics(
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
    pub fn transaction_payload(result: &SnapshotBenchmarkResult) -> String {
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

    fn block_interval_from_test_config(test_config: &TestConfig) -> Result<DevnetBlockInterval> {
        let block_time = test_config
            .parse_block_time()
            .map_err(eyre::Report::from)
            .wrap_err("failed to parse block_time from load-test configuration")?;
        if block_time == DevnetBlockInterval::TwoSeconds.duration() {
            return Ok(DevnetBlockInterval::TwoSeconds);
        }
        if block_time == DevnetBlockInterval::TwoHundredMilliseconds.duration() {
            return Ok(DevnetBlockInterval::TwoHundredMilliseconds);
        }
        eyre::bail!(
            "unsupported block_time {:?}; expected 2s or 200ms for snapshot benchmarks",
            block_time
        )
    }

    fn directory_name_or_default(path: &Path) -> String {
        path.file_name().and_then(|name| name.to_str()).unwrap_or("benchmark-output").to_string()
    }

    fn derived_run_id(benchmark_run: &str) -> String {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        format!("{benchmark_run}-{timestamp}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{Address, B256};
    use base_load_tests::{MetricsSummary, TestConfig, ThroughputMetrics};
    use clap::Parser;

    use super::{
        BenchmarkCli, BenchmarkCommand, SnapshotBenchmarkArgs, SnapshotBenchmarkResult,
        SnapshotBlockMetrics,
    };

    #[test]
    fn parses_snapshot_benchmark_defaults() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "snapshot",
            "--chain",
            "sepolia",
            "--builder-datadir",
            "/snapshot/builder",
            "--client-datadir",
            "/snapshot/client",
            "--load-test-config",
            "load.yaml",
            "--output-dir",
            "results/case-a",
            "--scenario",
            "example-scenario",
        ]);

        let BenchmarkCommand::Snapshot(args) = cli.command;
        assert_eq!(args.chain, "sepolia");
        assert_eq!(args.output_dir.to_string_lossy(), "results/case-a");
        assert_eq!(args.scenario, "example-scenario");
        assert_eq!(args.benchmark_run, "snapshot-throughput");
        assert!(args.run_id.is_none());
        assert!(args.client_version.is_none());
    }

    #[test]
    fn parses_snapshot_benchmark_output_and_run_id() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "snapshot",
            "--chain",
            "mainnet",
            "--builder-datadir",
            "/snapshot/builder",
            "--client-datadir",
            "/snapshot/client",
            "--load-test-config",
            "load.yaml",
            "--output-dir",
            "result-dir",
            "--scenario",
            "example-scenario",
            "--run-id",
            "manual-run-id",
        ]);

        let BenchmarkCommand::Snapshot(args) = cli.command;
        assert_eq!(args.output_dir.to_string_lossy(), "result-dir");
        assert_eq!(args.run_id.as_deref(), Some("manual-run-id"));
    }

    #[test]
    fn writes_visualizer_data_contract() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "snapshot",
            "--chain",
            "mainnet",
            "--builder-datadir",
            "/snapshot/builder",
            "--client-datadir",
            "/snapshot/client",
            "--load-test-config",
            "load.yaml",
            "--output-dir",
            "results/case-b",
            "--scenario",
            "blake2f-200ms",
            "--run-id",
            "run-123",
        ]);
        let BenchmarkCommand::Snapshot(mut args) = cli.command;
        args.client_version = Some("base/v0.0.0-test123".to_string());
        let output = tempfile::tempdir().unwrap();
        let output_name = output.path().file_name().unwrap().to_string_lossy().to_string();
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
transaction_submission_rpcs:
  - http://127.0.0.1:8545
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

        args.write_visualizer_bundle(output.path(), &output_name, "run-123", &result).unwrap();

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
        assert_eq!(SnapshotBenchmarkArgs::normalized_block_number(0, 2_000), 1.0);
        assert_eq!(SnapshotBenchmarkArgs::normalized_block_number(1, 2_000), 2.0);
        assert_eq!(SnapshotBenchmarkArgs::normalized_block_number(0, 200), 0.1);
        assert_eq!(SnapshotBenchmarkArgs::normalized_block_number(9, 200), 1.0);
        assert_eq!(SnapshotBenchmarkArgs::normalized_block_number(5_999, 200), 600.0);
    }
}
