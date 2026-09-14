//! Command-line orchestration for snapshot-backed benchmarks.

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_load_tests::{
    BaselineError, LoadTestDisplay, LoadTestExecutor, LoadTestRunHooks, LoadTestRunOptions,
    MetricsSummary, TestConfig,
};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};

use crate::{
    DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund, PrometheusBlockCollector,
    SnapshotBenchmarkReportConfig, SnapshotBenchmarkResult, SnapshotBlockMetrics,
    SnapshotChainConfig, SnapshotL2Stack, SystemTestStackBuilder, VisualizerMetadata,
    VisualizerRun,
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
    /// Aggregate selected snapshot run artifacts into one report metadata file.
    Aggregate(AggregateBenchmarkArgs),
}

/// Arguments for aggregating self-contained snapshot benchmark artifacts.
#[derive(Debug, Args)]
pub struct AggregateBenchmarkArgs {
    /// Common parent of the selected direct-child run directories. The command
    /// atomically writes this directory's metadata.json without moving or
    /// deleting any raw run artifacts.
    #[arg(long)]
    pub output_dir: PathBuf,
    /// One or more self-contained snapshot run output directories to include.
    #[arg(required = true, value_name = "RUN_OUTPUT_DIR")]
    pub run_output_dirs: Vec<PathBuf>,
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
    /// Maximum time to wait for graceful in-process stack shutdown after result artifacts have
    /// been written. Expiry terminates the process because snapshot datadirs are disposable.
    #[arg(long, default_value_t = 10)]
    pub shutdown_timeout_seconds: u64,
}

/// Wei minted to the benchmark's ephemeral funder in the first local descendant (1000 ETH).
const PREFUND_AMOUNT_WEI: u128 = 1_000_000_000_000_000_000_000;
const RESULT_FILE_NAME: &str = "benchmark-result.json";

impl BenchmarkCli {
    /// Runs the selected benchmark case.
    pub async fn run(self) -> Result<()> {
        let _progress = LoadTestDisplay::init_tracing();
        match self.command {
            BenchmarkCommand::Snapshot(args) => args.run().await,
            BenchmarkCommand::Aggregate(args) => args.run(),
        }
    }
}

impl AggregateBenchmarkArgs {
    /// Builds report metadata from selected raw run directories without modifying
    /// the source artifacts. One newest run is retained for every tag identity.
    pub fn run(self) -> Result<()> {
        let output_dir = fs::canonicalize(&self.output_dir).wrap_err_with(|| {
            format!("failed to resolve aggregate output directory {}", self.output_dir.display())
        })?;
        eyre::ensure!(
            output_dir.is_dir(),
            "aggregate output path is not a directory: {}",
            output_dir.display()
        );

        let mut selected = BTreeMap::<String, VisualizerRun>::new();
        for run_output_dir in self.run_output_dirs {
            let run_output_dir = fs::canonicalize(&run_output_dir).wrap_err_with(|| {
                format!("failed to resolve run output directory {}", run_output_dir.display())
            })?;
            eyre::ensure!(
                run_output_dir.is_dir(),
                "run output path is not a directory: {}",
                run_output_dir.display()
            );
            eyre::ensure!(
                run_output_dir.parent() == Some(output_dir.as_path()),
                "run output directory {} must be a direct child of aggregate output directory {}",
                run_output_dir.display(),
                output_dir.display(),
            );
            Self::validate_run_artifacts(&run_output_dir)?;
            let metadata_path = run_output_dir.join("metadata.json");
            let metadata: VisualizerMetadata = serde_json::from_slice(
                &fs::read(&metadata_path)
                    .wrap_err_with(|| format!("failed to read {}", metadata_path.display()))?,
            )
            .wrap_err_with(|| format!("failed to parse {}", metadata_path.display()))?;
            eyre::ensure!(
                metadata.runs.len() == 1,
                "{} must contain exactly one raw benchmark run, found {}",
                metadata_path.display(),
                metadata.runs.len(),
            );
            let run = metadata.runs.into_iter().next().expect("validated metadata run count");
            let output_name =
                run_output_dir.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
                    eyre::eyre!(
                        "run output directory has no UTF-8 basename: {}",
                        run_output_dir.display()
                    )
                })?;
            eyre::ensure!(
                run.output_dir == output_name,
                "run {} metadata outputDir {} does not match directory {}",
                run.id,
                run.output_dir,
                output_name,
            );
            let identity = Self::identity_key(&run)?;
            let created_at = Self::parse_created_at(&run)?;

            let replace = selected
                .get(&identity)
                .map(|existing| {
                    created_at
                        >= Self::parse_created_at(existing).expect("existing run was validated")
                })
                .unwrap_or(true);
            if replace {
                selected.insert(identity, run);
            }
        }

        let mut runs = selected.into_values().collect::<Vec<_>>();
        runs.sort_by(|left, right| {
            let left_time = Self::parse_created_at(left).expect("selected run was validated");
            let right_time = Self::parse_created_at(right).expect("selected run was validated");
            right_time.cmp(&left_time).then_with(|| right.id.cmp(&left.id))
        });
        Self::write_metadata_atomically(&output_dir, &VisualizerMetadata { runs })
    }

    fn validate_run_artifacts(run_output_dir: &Path) -> Result<()> {
        for file in [
            "metadata.json",
            "benchmark-result.json",
            "load-test-result.json",
            "metrics-sequencer.json",
            "metrics-validator.json",
        ] {
            let path = run_output_dir.join(file);
            eyre::ensure!(path.is_file(), "run artifact is missing: {}", path.display());
        }
        Ok(())
    }

    fn parse_created_at(run: &VisualizerRun) -> Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&run.created_at)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .wrap_err_with(|| format!("run {} has invalid createdAt {:?}", run.id, run.created_at))
    }

    fn identity_key(run: &VisualizerRun) -> Result<String> {
        serde_json::to_string(&run.test_config)
            .wrap_err("failed to serialize benchmark tag identity")
    }

    fn write_metadata_atomically(output_dir: &Path, metadata: &VisualizerMetadata) -> Result<()> {
        let destination = output_dir.join("metadata.json");
        let temporary = output_dir.join(format!(".metadata.json.{}.tmp", std::process::id()));
        fs::write(&temporary, serde_json::to_vec_pretty(metadata)?).wrap_err_with(|| {
            format!("failed to write aggregate metadata temporary file {}", temporary.display())
        })?;
        fs::rename(&temporary, &destination).wrap_err_with(|| {
            format!("failed to replace aggregate metadata {}", destination.display())
        })?;
        println!(
            "aggregated {} selected benchmark run(s): {}",
            metadata.runs.len(),
            destination.display()
        );
        Ok(())
    }
}

impl SnapshotBenchmarkArgs {
    /// Starts the devnet, runs the load test, writes results, and shuts down.
    pub async fn run(self) -> Result<()> {
        let test_config = TestConfig::load(&self.load_test_config)
            .wrap_err("failed to load benchmark load-test configuration")?;
        let block_interval = Self::block_interval_from_test_config(&test_config)?;
        let funder_key = PrivateKeySigner::random();
        let client_version = self.client_version.clone().ok_or_else(|| {
            eyre::eyre!("--client-version or BASE_BENCH_CLIENT_VERSION is required")
        })?;
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

        Self::reset_output_dir(&self.output_dir)?;
        let result_path = self.output_dir.join(RESULT_FILE_NAME);
        let output_name = Self::output_directory_name(&self.output_dir);
        let run_id =
            self.run_id.clone().unwrap_or_else(|| Self::derived_run_id(&self.benchmark_run));
        let report = SnapshotBenchmarkReportConfig::new(
            self.output_dir.clone(),
            output_name,
            run_id,
            self.benchmark_run.clone(),
            self.scenario.clone(),
            client_version,
        );

        eyre::ensure!(
            self.shutdown_timeout_seconds > 0,
            "shutdown timeout must be greater than zero"
        );
        let mut stack = SystemTestStackBuilder::new()
            .with_devnet_config(devnet)
            .build_snapshot_sequencer()
            .await?;
        let benchmark_result =
            self.execute(&mut stack, test_config, block_interval, funder_key).await;

        // Persist the report before teardown. Reth may have a non-cancellable serial state-root
        // fallback still running after consensus shutdown, and dropping its runtime waits for
        // blocking work to finish. The snapshot datadirs are disposable, so teardown is bounded
        // separately after preserving the useful benchmark output.
        let output_result = (|| -> Result<()> {
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
            report.write_visualizer_bundle(&result)?;
            println!("benchmark result: {}", result_path.display());
            std::io::stdout().flush().wrap_err("failed to flush benchmark result output")?;
            Ok(())
        })();

        if let Err(error) = &output_result {
            eprintln!("benchmark result processing failed before shutdown: {error:?}");
            let _ = std::io::stderr().flush();
        }
        let shutdown_result =
            Self::shutdown_with_deadline(stack, self.shutdown_timeout_seconds).await;
        output_result?;
        shutdown_result
    }

    /// Gracefully shuts down the stack, but terminates the process if runtime destruction remains
    /// blocked after the deadline. A native thread is used because the Tokio runtime itself may be
    /// the component waiting on a non-cancellable blocking state-root task.
    async fn shutdown_with_deadline(stack: SnapshotL2Stack, timeout_seconds: u64) -> Result<()> {
        let complete = Arc::new(AtomicBool::new(false));
        let watchdog_complete = Arc::clone(&complete);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(timeout_seconds));
            if !watchdog_complete.load(Ordering::Acquire) {
                eprintln!(
                    "snapshot stack shutdown exceeded {timeout_seconds}s after result output; forcing failed process exit"
                );
                let _ = std::io::stderr().flush();
                // The result path was explicitly flushed before teardown. A graceful shutdown is
                // already stuck, and the snapshot datadirs are disposable, so report the abnormal
                // teardown to the caller instead of waiting indefinitely for runtime destructors.
                std::process::exit(1);
            }
        });

        let result = stack.shutdown().await;
        complete.store(true, Ordering::Release);
        result
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
        // Each snapshot benchmark starts a fresh in-process builder, so its txpool
        // is not persisted in the snapshot datadir. Do not require optional admin
        // txpool RPC wiring during setup; nonce recovery remains load-test-owned.
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

    /// Derives the snapshot cadence from the load-test configuration.
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

    fn output_directory_name(path: &Path) -> String {
        path.file_name().and_then(|name| name.to_str()).unwrap_or("benchmark-output").to_string()
    }

    /// Removes stale artifacts so a failed rerun cannot be mistaken for an older completed run.
    fn reset_output_dir(path: &Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_dir_all(path).wrap_err_with(|| {
                format!("failed to clear benchmark output directory {}", path.display())
            })?;
        }
        std::fs::create_dir_all(path).wrap_err_with(|| {
            format!("failed to create benchmark output directory {}", path.display())
        })
    }

    fn derived_run_id(benchmark_run: &str) -> String {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        format!("{benchmark_run}-{timestamp}")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;

    use super::{AggregateBenchmarkArgs, BenchmarkCli, BenchmarkCommand, SnapshotBenchmarkArgs};

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

        let BenchmarkCommand::Snapshot(args) = cli.command else {
            panic!("expected snapshot benchmark command");
        };
        assert_eq!(args.chain, "sepolia");
        assert_eq!(args.output_dir.to_string_lossy(), "results/case-a");
        assert_eq!(args.scenario, "example-scenario");
        assert_eq!(args.benchmark_run, "snapshot-throughput");
        assert!(args.run_id.is_none());
        assert!(args.client_version.is_none());
        assert_eq!(args.shutdown_timeout_seconds, 10);
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
            "--shutdown-timeout-seconds",
            "30",
        ]);

        let BenchmarkCommand::Snapshot(args) = cli.command else {
            panic!("expected snapshot benchmark command");
        };
        assert_eq!(args.output_dir.to_string_lossy(), "result-dir");
        assert_eq!(args.run_id.as_deref(), Some("manual-run-id"));
        assert_eq!(args.shutdown_timeout_seconds, 30);
    }

    #[test]
    fn reset_output_dir_removes_stale_artifacts() {
        let output = tempfile::tempdir().unwrap();
        let stale = output.path().join("metadata.json");
        fs::write(&stale, "old completed run").unwrap();

        SnapshotBenchmarkArgs::reset_output_dir(output.path()).unwrap();

        assert!(output.path().is_dir());
        assert!(!stale.exists());
    }

    fn write_aggregate_run(
        root: &std::path::Path,
        id: &str,
        scenario: &str,
        client_version: &str,
        created_at: &str,
    ) -> std::path::PathBuf {
        let output = root.join(id);
        std::fs::create_dir_all(&output).unwrap();
        for artifact in [
            "benchmark-result.json",
            "load-test-result.json",
            "metrics-sequencer.json",
            "metrics-validator.json",
        ] {
            std::fs::write(output.join(artifact), "{}\n").unwrap();
        }
        let metadata = serde_json::json!({
            "runs": [{
                "id": id,
                "sourceFile": "base-sepolia-snapshot",
                "outputDir": id,
                "testName": "Base Sepolia snapshot throughput",
                "testDescription": "test",
                "testConfig": {
                    "BenchmarkRun": "sepolia-transfer-100mgas",
                    "Scenario": scenario,
                    "ChainId": 84532,
                    "BlockTimeMilliseconds": 200,
                    "GasLimit": 400000000,
                    "NodeType": "base-reth-node",
                    "TransactionPayload": "transfer",
                    "ClientVersion": client_version
                },
                "result": {
                    "success": true,
                    "complete": true,
                    "clientVersion": client_version,
                    "sequencerMetrics": {"gasPerSecond": 1.0},
                    "validatorMetrics": {"gasPerSecond": 1.0},
                    "artifacts": {"loadTestResult": "load-test-result.json"}
                },
                "createdAt": created_at
            }]
        });
        std::fs::write(output.join("metadata.json"), serde_json::to_vec(&metadata).unwrap())
            .unwrap();
        output
    }

    #[test]
    fn parses_aggregate_benchmark_command() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "aggregate",
            "--output-dir",
            "results",
            "results/run-a",
        ]);
        let BenchmarkCommand::Aggregate(args) = cli.command else {
            panic!("expected aggregate benchmark command");
        };
        assert_eq!(args.output_dir, std::path::PathBuf::from("results"));
        assert_eq!(args.run_output_dirs, vec![std::path::PathBuf::from("results/run-a")]);
    }

    #[test]
    fn aggregate_keeps_latest_run_per_normalized_tag_set() {
        let root = tempfile::tempdir().unwrap();
        let early = write_aggregate_run(
            root.path(),
            "transfer-early",
            "transfer-100mgas-200ms",
            "base/a",
            "2026-09-03T00:00:00.000Z",
        );
        let latest = write_aggregate_run(
            root.path(),
            "transfer-latest",
            "transfer-100mgas-200ms",
            "base/a",
            "2026-09-03T00:02:00.000Z",
        );
        let distinct_scenario = write_aggregate_run(
            root.path(),
            "swap",
            "swap-100mgas-200ms @ 2026-09-02T23:00:00.000Z",
            "base/a",
            "2026-09-03T00:01:00.000Z",
        );
        let distinct_version = write_aggregate_run(
            root.path(),
            "new-version",
            "transfer-100mgas-200ms",
            "base/b",
            "2026-09-03T00:03:00.000Z",
        );

        AggregateBenchmarkArgs {
            output_dir: root.path().to_path_buf(),
            run_output_dirs: vec![early, latest, distinct_scenario, distinct_version],
        }
        .run()
        .unwrap();

        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("metadata.json")).unwrap())
                .unwrap();
        let runs = metadata["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0]["id"], "new-version");
        assert_eq!(runs[1]["id"], "transfer-latest");
        assert_eq!(runs[2]["id"], "swap");
        assert_eq!(runs[1]["testConfig"]["Scenario"], "transfer-100mgas-200ms");
        assert_eq!(
            runs[2]["testConfig"]["Scenario"],
            "swap-100mgas-200ms @ 2026-09-02T23:00:00.000Z"
        );
        assert!(root.path().join("transfer-early").is_dir());
    }

    #[test]
    fn aggregate_rejects_invalid_artifacts_without_replacing_metadata() {
        let root = tempfile::tempdir().unwrap();
        let prior = b"{\n  \"runs\": [\n    {\"id\": \"preserved\"}\n  ]\n}\n";
        std::fs::write(root.path().join("metadata.json"), prior).unwrap();
        let invalid = root.path().join("invalid");
        std::fs::create_dir_all(&invalid).unwrap();
        std::fs::write(invalid.join("metadata.json"), "{\"runs\": []}").unwrap();

        let error = AggregateBenchmarkArgs {
            output_dir: root.path().to_path_buf(),
            run_output_dirs: vec![invalid],
        }
        .run()
        .unwrap_err();
        assert!(error.to_string().contains("run artifact is missing"));
        assert_eq!(std::fs::read(root.path().join("metadata.json")).unwrap(), prior);
    }
}
