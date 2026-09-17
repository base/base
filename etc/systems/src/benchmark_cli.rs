//! Command-line orchestration for fresh-devnet and snapshot-backed benchmarks.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    num::NonZeroU64,
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
use base_common_precompiles::ActivationFeature;
use base_load_tests::{
    BaselineError, LoadTestDisplay, LoadTestExecutor, LoadTestRunHooks, LoadTestRunOptions,
    MetricsSummary, TestConfig, TxTypeConfig,
};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::{
    ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_5, B20PrecompileClient, DevnetBlockInterval, DevnetConfig,
    DevnetL2State, DevnetPrefund, PrometheusBlockCollector, SnapshotBenchmarkReportConfig,
    SnapshotBenchmarkResult, SnapshotBlockMetrics, SnapshotChainConfig, SnapshotL2Stack,
    SystemTestStack, SystemTestStackBuilder, VisualizerMetadata, VisualizerRun,
    VisualizerRunResult, VisualizerSequencerMetrics, VisualizerValidatorMetrics,
};

/// Base benchmark launcher.
#[derive(Debug, Parser)]
#[command(author, version, about = "Benchmark a Base development network")]
pub struct BenchmarkCli {
    /// Benchmark target. Omitting it runs the default fresh-devnet transfer benchmark.
    #[command(subcommand)]
    pub command: Option<BenchmarkCommand>,
}

/// Supported benchmark targets.
#[derive(Debug, Subcommand)]
pub enum BenchmarkCommand {
    /// Run one benchmark or a workload suite against fresh temporary local devnets.
    Local(LocalBenchmarkArgs),
    /// Run one load test against a Base snapshot continuation.
    Snapshot(Box<SnapshotBenchmarkArgs>),
    /// Aggregate selected snapshot run artifacts into one report metadata file.
    Aggregate(AggregateBenchmarkArgs),
}

/// Arguments for a fresh-devnet benchmark.
#[derive(Debug, Args, Default)]
pub struct LocalBenchmarkArgs {
    /// Load-test YAML. Its endpoint fields are replaced with fresh-devnet endpoints.
    #[arg(long, conflicts_with = "workload_config")]
    pub load_test_config: Option<PathBuf>,
    /// YAML workload suite. Each workload runs against its own empty temporary devnet.
    #[arg(long, conflicts_with = "load_test_config")]
    pub workload_config: Option<PathBuf>,
    /// Directory for a single result or a workload-suite visualizer bundle.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Stable client build label written into workload-suite visualizer metadata.
    #[arg(long, env = "BASE_BENCH_CLIENT_VERSION")]
    pub client_version: Option<String>,
}

/// One entry in a fresh-devnet workload suite.
#[derive(Debug, Deserialize)]
pub struct LocalBenchmarkWorkload {
    /// Stable workload identifier and output-directory name.
    pub workload: String,
    /// Human-readable payload label for the visualizer.
    pub transaction_payload: String,
    /// Load-test settings for this workload.
    #[serde(flatten)]
    pub test_config: TestConfig,
}

/// YAML configuration for a fresh-devnet workload suite.
#[derive(Debug, Deserialize)]
pub struct LocalBenchmarkWorkloadConfig {
    /// Workloads to run serially. Every workload receives a newly initialized devnet.
    pub benchmark_workloads: Vec<LocalBenchmarkWorkload>,
}

/// Machine-readable outcome for one fresh-devnet workload.
#[derive(Debug, Serialize)]
pub struct LocalBenchmarkWorkloadResult {
    /// Stable workload identifier.
    pub workload: String,
    /// Human-readable payload label.
    pub transaction_payload: String,
    /// Relative output directory when a load-test sidecar was written.
    pub output_dir: String,
    /// Whether the load test completed without a terminal error.
    pub success: bool,
    /// Terminal error, when startup or execution failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Machine-readable outcomes for a fresh-devnet workload suite.
#[derive(Debug, Serialize)]
pub struct LocalBenchmarkWorkloadResults {
    /// Results in the same order as the workload configuration.
    pub runs: Vec<LocalBenchmarkWorkloadResult>,
}

/// In-memory outcome from one local load-test invocation.
#[derive(Debug)]
pub struct LocalBenchmarkResult {
    /// Native load-test metrics, written to `load-test-result.json` when requested.
    pub summary: MetricsSummary,
    /// A terminal load-test error preserved after its metrics were written.
    pub error: Option<String>,
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
    /// Block gas limit for locally produced descendants. Defaults to 10 Ggas for 2s blocks and
    /// 1 Ggas for 200ms blocks.
    #[arg(long)]
    pub block_gas_limit: Option<NonZeroU64>,
    /// Maximum time to wait for graceful shutdown after writing results. Zero terminates the
    /// process immediately because snapshot datadirs are disposable.
    #[arg(long, default_value_t = 0)]
    pub shutdown_timeout_seconds: u64,
}

/// Wei minted to the benchmark's ephemeral funder in the first local descendant (1000 ETH).
const PREFUND_AMOUNT_WEI: u128 = 1_000_000_000_000_000_000_000;
/// Use the full block gas limit as the EIP-1559 target in snapshot benchmarks, so synthetic
/// benchmark load cannot raise the base fee and strand already-submitted transaction nonce lanes.
const SNAPSHOT_BENCHMARK_EIP1559_ELASTICITY: u32 = 1;
const RESULT_FILE_NAME: &str = "benchmark-result.json";
const LOCAL_RESULT_FILE_NAME: &str = "load-test-result.json";
const FRESH_DEVNET_BLOCK_TIME: Duration = Duration::from_secs(2);
const FRESH_DEVNET_CHAIN_ID: u64 = 84_538_453;
const FRESH_DEVNET_AZUL_ACTIVATION_BLOCK: u64 = 0;
const FRESH_DEVNET_BERYL_ACTIVATION_BLOCK: u64 = 3;
const FRESH_DEVNET_BERYL_READY_BLOCK: u64 = FRESH_DEVNET_BERYL_ACTIVATION_BLOCK + 1;
const FRESH_DEVNET_BERYL_READY_TIMEOUT: Duration = Duration::from_secs(30);

impl BenchmarkCli {
    /// Runs the selected benchmark case.
    pub async fn run(self) -> Result<()> {
        let _progress = LoadTestDisplay::init_tracing();
        match self.command {
            None => LocalBenchmarkArgs::default().run().await,
            Some(BenchmarkCommand::Local(args)) => args.run().await,
            Some(BenchmarkCommand::Snapshot(args)) => args.run().await,
            Some(BenchmarkCommand::Aggregate(args)) => args.run(),
        }
    }
}

impl LocalBenchmarkArgs {
    /// Runs either one configured local benchmark or a suite of isolated local benchmarks.
    pub async fn run(self) -> Result<()> {
        if let Some(workload_config) = &self.workload_config {
            let output_dir = self.output_dir.as_deref().ok_or_else(|| {
                eyre::eyre!("--output-dir is required when --workload-config is provided")
            })?;
            return self.run_workload_suite(workload_config, output_dir).await;
        }

        let test_config = self
            .load_test_config
            .as_ref()
            .map(TestConfig::load)
            .transpose()
            .wrap_err("failed to load fresh-devnet benchmark configuration")?
            .unwrap_or_default();
        let result = self.run_one(test_config).await?;
        if let Some(error) = result.error {
            eyre::bail!(error)
        }
        Ok(())
    }

    /// Starts an isolated fresh devnet, executes one load-test configuration, and tears it down.
    ///
    /// The configuration's RPC endpoints are deliberately ignored: every run targets the dynamic
    /// endpoints assigned to its newly created devnet. A B-20 workload schedules Azul and Beryl,
    /// then activates the B-20 asset feature before creating its token fixtures.
    pub async fn run_one(&self, mut test_config: TestConfig) -> Result<LocalBenchmarkResult> {
        let devnet = DevnetConfig::standard();
        let chain_id = devnet.l2_chain_id;
        let funder_key = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)
            .wrap_err("failed to construct the fresh-devnet funder key")?;
        let requires_b20_activation = Self::requires_beryl(&test_config);
        let mut stack_builder = SystemTestStackBuilder::new().with_devnet_config(devnet);
        if requires_b20_activation {
            stack_builder = stack_builder
                .with_base_azul_activation_block(FRESH_DEVNET_AZUL_ACTIVATION_BLOCK)
                .with_base_beryl_activation_block(FRESH_DEVNET_BERYL_ACTIVATION_BLOCK);
        }
        let stack = stack_builder.build().await?;

        if requires_b20_activation {
            Self::activate_b20_asset_feature(&stack, chain_id).await?;
        }

        println!("running benchmark against a fresh temporary devnet");
        let run_result: Result<LocalBenchmarkResult> = async {
            let builder_rpc = stack.l2_rpc_url()?;
            let client_rpc = stack.l2_client_rpc_url()?;
            let flashblocks_ws = stack
                .l2_stack()
                .builder()
                .flashblocks_url()
                .parse()
                .wrap_err("invalid fresh-devnet Flashblocks URL")?;
            test_config.transaction_submission_rpcs = vec![builder_rpc];
            test_config.query_rpc = Some(client_rpc);
            test_config.txpool_nodes.clear();
            test_config.flashblocks_ws = Some(flashblocks_ws);
            test_config.chain_id = Some(chain_id);

            let load_config = test_config.to_load_config(None)?;
            eyre::ensure!(
                load_config.block_time == FRESH_DEVNET_BLOCK_TIME,
                "fresh devnet uses a 2s block time, but the workload requests {}",
                test_config.block_time
            );
            let output = LoadTestExecutor::run_prepared(
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
            .await?;
            println!("{}", serde_json::to_string_pretty(&output.summary)?);
            self.write_result(&output.summary)?;
            Ok(LocalBenchmarkResult {
                summary: output.summary,
                error: output.run_error.map(|error| error.to_string()),
            })
        }
        .await;

        let shutdown_result = stack.shutdown().await;
        let result = run_result?;
        shutdown_result?;
        Ok(result)
    }

    /// Runs every workload in a YAML suite serially and writes a base/benchmark visualizer bundle.
    pub async fn run_workload_suite(
        &self,
        workload_config_path: &Path,
        output_dir: &Path,
    ) -> Result<()> {
        let workload_config = Self::load_workload_config(workload_config_path)?;
        eyre::ensure!(
            !workload_config.benchmark_workloads.is_empty(),
            "fresh-devnet workload configuration contains no benchmark_workloads"
        );
        fs::create_dir_all(output_dir).wrap_err_with(|| {
            format!(
                "failed to create fresh-devnet benchmark output directory {}",
                output_dir.display()
            )
        })?;

        let client_version =
            self.client_version.clone().unwrap_or_else(|| "base/unknown".to_string());
        let mut result_runs = Vec::with_capacity(workload_config.benchmark_workloads.len());
        let mut visualizer_runs = Vec::with_capacity(workload_config.benchmark_workloads.len());
        let mut workload_names = BTreeSet::new();

        for workload in workload_config.benchmark_workloads {
            eyre::ensure!(
                workload_names.insert(workload.workload.clone()),
                "fresh-devnet workload configuration contains duplicate workload {:?}",
                workload.workload
            );
            let output_dir_name = format!("fresh-devnet-{}", workload.workload);
            let workload_output_dir = Self::workload_output_dir(output_dir, &output_dir_name)?;
            let result = Self { output_dir: Some(workload_output_dir), ..Self::default() }
                .run_one(workload.test_config)
                .await;

            let (summary, success, error) = match result {
                Ok(result) => {
                    let success = result.error.is_none();
                    (Some(result.summary), success, result.error)
                }
                Err(error) => (None, false, Some(format!("{error:?}"))),
            };
            let complete = summary.is_some();
            let gas_per_second =
                summary.as_ref().map(|summary| summary.throughput.gps).unwrap_or_default();

            result_runs.push(LocalBenchmarkWorkloadResult {
                workload: workload.workload.clone(),
                transaction_payload: workload.transaction_payload.clone(),
                output_dir: output_dir_name.clone(),
                success,
                error: error.clone(),
            });
            visualizer_runs.push(VisualizerRun {
                id: format!("fresh-devnet-{}-{}", workload.workload, Utc::now().timestamp_millis()),
                source_file: "base-fresh-devnet".to_string(),
                output_dir: output_dir_name,
                test_name: format!("Base fresh-devnet {}", workload.workload),
                test_description: format!(
                    "{} load test against a newly initialized empty Base devnet",
                    workload.transaction_payload
                ),
                test_config: BTreeMap::from([
                    (
                        "BenchmarkRun".to_string(),
                        serde_json::Value::String("fresh-devnet".to_string()),
                    ),
                    ("Scenario".to_string(), serde_json::Value::String(workload.workload)),
                    ("ChainId".to_string(), FRESH_DEVNET_CHAIN_ID.into()),
                    ("BlockTimeMilliseconds".to_string(), 2_000.into()),
                    ("NodeType".to_string(), serde_json::Value::String("fresh-devnet".to_string())),
                    (
                        "TransactionPayload".to_string(),
                        serde_json::Value::String(workload.transaction_payload),
                    ),
                    (
                        "ClientVersion".to_string(),
                        serde_json::Value::String(client_version.clone()),
                    ),
                ]),
                result: VisualizerRunResult {
                    success,
                    complete,
                    client_version: client_version.clone(),
                    // This is the end-to-end load-test GPS, not a per-node Prometheus scrape.
                    // It fills the visualizer's headline field while the detailed load-test page
                    // renders the native MetricsSummary sidecar.
                    sequencer_metrics: VisualizerSequencerMetrics { gas_per_second },
                    validator_metrics: VisualizerValidatorMetrics { gas_per_second },
                    artifacts: if complete {
                        BTreeMap::from([(
                            "loadTestResult".to_string(),
                            LOCAL_RESULT_FILE_NAME.to_string(),
                        )])
                    } else {
                        BTreeMap::new()
                    },
                },
                created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            });
        }

        let suite_results = LocalBenchmarkWorkloadResults { runs: result_runs };
        Self::write_json(
            &output_dir.join("suite-results.json"),
            &suite_results,
            "fresh-devnet workload results",
        )?;
        Self::write_json(
            &output_dir.join("metadata.json"),
            &VisualizerMetadata { runs: visualizer_runs },
            "fresh-devnet visualizer metadata",
        )?;

        let failed = suite_results.runs.iter().filter(|run| !run.success).count();
        eyre::ensure!(failed == 0, "{failed} fresh-devnet benchmark workload(s) failed");
        Ok(())
    }

    /// Writes a single load-test sidecar for a local benchmark when an output directory was set.
    pub fn write_result(&self, summary: &MetricsSummary) -> Result<()> {
        let Some(output_dir) = &self.output_dir else { return Ok(()) };
        let result_path = output_dir.join(LOCAL_RESULT_FILE_NAME);
        Self::write_json(&result_path, summary, "fresh-devnet load-test result")?;
        println!("benchmark result: {}", result_path.display());
        Ok(())
    }

    /// Loads a fresh-devnet workload suite from YAML.
    pub fn load_workload_config(path: &Path) -> Result<LocalBenchmarkWorkloadConfig> {
        let contents = fs::read_to_string(path).wrap_err_with(|| {
            format!("failed to read workload configuration {}", path.display())
        })?;
        serde_yaml::from_str(&contents)
            .wrap_err_with(|| format!("failed to parse workload configuration {}", path.display()))
    }

    /// Returns the output directory for one workload after validating that it cannot escape root.
    pub fn workload_output_dir(root: &Path, workload: &str) -> Result<PathBuf> {
        eyre::ensure!(
            !workload.is_empty()
                && workload != "."
                && workload != ".."
                && !workload.contains('/')
                && !workload.contains('\\'),
            "workload must be a single directory name: {workload:?}"
        );
        Ok(root.join(workload))
    }

    /// Writes one JSON artifact and ensures its parent directory exists.
    pub fn write_json<T: Serialize>(path: &Path, value: &T, artifact: &str) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            eyre::eyre!("{artifact} path has no parent directory: {}", path.display())
        })?;
        fs::create_dir_all(parent).wrap_err_with(|| {
            format!("failed to create {artifact} directory {}", parent.display())
        })?;
        fs::write(path, serde_json::to_vec_pretty(value)?)
            .wrap_err_with(|| format!("failed to write {artifact} {}", path.display()))
    }

    /// Returns whether a workload needs Base Beryl enabled in its empty devnet.
    pub fn requires_beryl(test_config: &TestConfig) -> bool {
        test_config
            .transactions
            .iter()
            .any(|transaction| matches!(transaction.tx_type, TxTypeConfig::B20))
    }

    /// Waits for Beryl and activates the B-20 asset feature with the devnet sequencer account.
    pub async fn activate_b20_asset_feature(stack: &SystemTestStack, chain_id: u64) -> Result<()> {
        let provider = stack.l2_builder_provider()?;
        tokio::time::timeout(FRESH_DEVNET_BERYL_READY_TIMEOUT, async {
            loop {
                if provider.get_block_number().await? >= FRESH_DEVNET_BERYL_READY_BLOCK {
                    return Ok::<_, eyre::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .wrap_err("timed out waiting for Beryl to activate on the fresh devnet")??;

        let activation_admin = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_5.private_key)
            .wrap_err("failed to construct the fresh-devnet Beryl activation-admin key")?;
        B20PrecompileClient::new(&provider, &activation_admin, chain_id)
            .activate_feature(ActivationFeature::B20Asset.id())
            .await
            .wrap_err("failed to activate the B-20 asset feature on the fresh devnet")
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
        snapshot.block_gas_limit = self.block_gas_limit.map(NonZeroU64::get);
        snapshot.eip1559_elasticity_override = Some(SNAPSHOT_BENCHMARK_EIP1559_ELASTICITY);
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

        if self.shutdown_timeout_seconds == 0 {
            // The benchmark artifacts or error diagnostics are complete and flushed. Exit from
            // inside the async entrypoint so neither the stack nor the outer Tokio runtime runs
            // destructors that can wait for non-cancellable Reth work or database cleanup.
            std::process::exit(if output_result.is_ok() { 0 } else { 1 });
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
    use std::{fs, num::NonZeroU64};

    use base_load_tests::MetricsSummary;
    use clap::Parser;

    use super::{
        AggregateBenchmarkArgs, BenchmarkCli, BenchmarkCommand, LocalBenchmarkArgs,
        LocalBenchmarkWorkloadConfig, SnapshotBenchmarkArgs,
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

        let Some(BenchmarkCommand::Snapshot(args)) = cli.command else {
            panic!("expected snapshot benchmark command");
        };
        assert_eq!(args.chain, "sepolia");
        assert_eq!(args.output_dir.to_string_lossy(), "results/case-a");
        assert_eq!(args.scenario, "example-scenario");
        assert_eq!(args.benchmark_run, "snapshot-throughput");
        assert!(args.run_id.is_none());
        assert!(args.client_version.is_none());
        assert!(args.block_gas_limit.is_none());
        assert_eq!(args.shutdown_timeout_seconds, 0);
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
            "--block-gas-limit",
            "12000000000",
        ]);

        let Some(BenchmarkCommand::Snapshot(args)) = cli.command else {
            panic!("expected snapshot benchmark command");
        };
        assert_eq!(args.output_dir.to_string_lossy(), "result-dir");
        assert_eq!(args.run_id.as_deref(), Some("manual-run-id"));
        assert_eq!(args.block_gas_limit.map(NonZeroU64::get), Some(12_000_000_000));
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
        let Some(BenchmarkCommand::Aggregate(args)) = cli.command else {
            panic!("expected aggregate benchmark command");
        };
        assert_eq!(args.output_dir, std::path::PathBuf::from("results"));
        assert_eq!(args.run_output_dirs, vec![std::path::PathBuf::from("results/run-a")]);
    }

    #[test]
    fn defaults_to_fresh_local_benchmark_without_arguments() {
        let cli = BenchmarkCli::parse_from(["base-bench"]);

        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_explicit_fresh_local_benchmark() {
        let cli = BenchmarkCli::parse_from(["base-bench", "local"]);

        let Some(BenchmarkCommand::Local(args)) = cli.command else {
            panic!("expected local benchmark command");
        };
        assert!(args.load_test_config.is_none());
        assert!(args.workload_config.is_none());
        assert!(args.output_dir.is_none());
    }

    #[test]
    fn parses_fresh_devnet_workload_suite() {
        let cli = BenchmarkCli::parse_from([
            "base-bench",
            "local",
            "--workload-config",
            "etc/benchmarks/fresh-devnet.yml",
            "--output-dir",
            "results/fresh-devnet",
            "--client-version",
            "base/test",
        ]);

        let Some(BenchmarkCommand::Local(args)) = cli.command else {
            panic!("expected local benchmark command");
        };
        assert_eq!(
            args.workload_config,
            Some(std::path::PathBuf::from("etc/benchmarks/fresh-devnet.yml"))
        );
        assert_eq!(args.output_dir, Some(std::path::PathBuf::from("results/fresh-devnet")));
        assert_eq!(args.client_version.as_deref(), Some("base/test"));
    }

    #[test]
    fn parses_checked_in_fresh_devnet_workload_configuration() {
        let config: LocalBenchmarkWorkloadConfig =
            serde_yaml::from_str(include_str!("../../benchmarks/fresh-devnet.yml")).unwrap();

        assert_eq!(config.benchmark_workloads.len(), 4);
        assert_eq!(config.benchmark_workloads[0].workload, "b20-transfer");
        assert_eq!(config.benchmark_workloads[0].transaction_payload, "b20-transfer-existing");
        assert_eq!(config.benchmark_workloads[0].test_config.sender_count, 400);
        assert_eq!(config.benchmark_workloads[0].test_config.funding_amount, "100000000000000000");
        assert_eq!(config.benchmark_workloads[1].workload, "eth-new");
        assert_eq!(config.benchmark_workloads[1].test_config.sender_count, 1000);
        assert_eq!(config.benchmark_workloads[2].workload, "eth-existing");
        assert_eq!(config.benchmark_workloads[3].workload, "blake2f-50000");
    }

    #[test]
    fn writes_local_load_test_sidecar_when_output_is_requested() {
        let output = tempfile::tempdir().unwrap();
        let args = LocalBenchmarkArgs {
            output_dir: Some(output.path().to_path_buf()),
            ..Default::default()
        };
        let summary = MetricsSummary { measurement_block_count: 10, ..Default::default() };

        args.write_result(&summary).unwrap();

        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(output.path().join("load-test-result.json")).unwrap())
                .unwrap();
        assert_eq!(written["measurement_block_count"], 10);
    }

    #[test]
    fn rejects_workload_names_that_escape_the_result_root() {
        let root = tempfile::tempdir().unwrap();

        assert!(LocalBenchmarkArgs::workload_output_dir(root.path(), "../escape").is_err());
        assert!(LocalBenchmarkArgs::workload_output_dir(root.path(), "b20-transfer").is_ok());
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
