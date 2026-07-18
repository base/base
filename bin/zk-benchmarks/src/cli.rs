//! CLI argument parsing and execution for the ZK benchmark binary.

use std::path::PathBuf;

use base_cli_utils::RuntimeManager;
use base_load_tests::{
    LoadTestCleanupSummary, LoadTestDisplay, LoadTestExecutor, LoadTestRunOptions, TestConfig,
};
use base_prover_service_protocol::ZkBackend;
use base_zk_benchmarks::{ZkBenchConfig, ZkBenchRunner, ZkBenchSummary};
use clap::{Args, Parser, ValueEnum};
use eyre::{Result, bail};

/// The Base ZK benchmark CLI.
#[derive(Parser, Clone, Debug)]
#[command(author, version = env!("CARGO_PKG_VERSION"), about = "Base ZK benchmarks")]
pub(crate) struct Cli {
    /// ZK benchmark arguments.
    #[command(flatten)]
    args: ZkBenchArgs,
}

impl Cli {
    /// Runs the selected benchmark.
    pub(crate) fn run(self) -> Result<()> {
        RuntimeManager::new()
            .tokio_runtime()?
            .block_on(async move { run_zk_benchmark(self.args).await })
    }
}

/// ZK benchmark command arguments.
#[derive(Args, Clone, Debug)]
struct ZkBenchArgs {
    /// ZK proof backend.
    #[arg(long, value_enum)]
    mode: ZkBackendArg,

    /// Rollup node RPC URL. This is the op-node RPC, not the L2 execution RPC.
    #[arg(
        long = "rollup-rpc-url",
        env = "ROLLUP_RPC_URL",
        default_value = "http://localhost:8649"
    )]
    rollup_rpc_url: url::Url,

    /// Prover-service requester JSON-RPC URL.
    #[arg(
        long = "prover-service-url",
        env = "PROVER_SERVICE_URL",
        default_value = "http://localhost:9000"
    )]
    prover_service_url: url::Url,

    /// Load test YAML configuration.
    #[arg(value_name = "CONFIG")]
    config: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ZkBackendArg {
    DryRun,
    Cluster,
    Network,
}

impl From<ZkBackendArg> for ZkBackend {
    fn from(mode: ZkBackendArg) -> Self {
        match mode {
            ZkBackendArg::DryRun => Self::DryRun,
            ZkBackendArg::Cluster => Self::Cluster,
            ZkBackendArg::Network => Self::Network,
        }
    }
}

async fn run_zk_benchmark(args: ZkBenchArgs) -> Result<()> {
    let _mp = LoadTestDisplay::init_tracing();

    if !args.config.exists() {
        bail!("config file not found: {}", args.config.display());
    }

    let test_config = TestConfig::load(&args.config)?;
    let zk_config = ZkBenchConfig::new(
        args.rollup_rpc_url.clone(),
        args.prover_service_url.clone(),
        args.mode.into(),
    );

    println!("=== Base ZK Benchmark Runner ===");
    println!("Config: {}", args.config.display());
    println!(
        "Backend: {} | Rollup RPC: {} | Prover: {}",
        zk_config.zk_backend, zk_config.rollup_rpc_url, zk_config.prover_url
    );
    println!();

    let output = LoadTestExecutor::run(
        test_config,
        LoadTestRunOptions { install_signal_handler: true, ..Default::default() },
    )
    .await?;

    let summary = output.summary;
    if let Ok(output_path) = std::env::var("LOAD_TEST_OUTPUT") {
        match summary.to_json() {
            Ok(json) => match std::fs::write(&output_path, &json) {
                Ok(()) => println!("Load test results written to {output_path}"),
                Err(e) => {
                    eprintln!("Warning: failed to write load test results to {output_path}: {e}")
                }
            },
            Err(e) => eprintln!("Warning: failed to serialize load test results: {e}"),
        }
    }
    print_cleanup_warnings(&output.cleanup);
    if let Some(error) = output.run_error {
        return Err(error.into());
    }

    println!();
    println!("Running ZK benchmark...");
    let zk_summary = ZkBenchRunner::run(&summary, zk_config).await?;
    print_zk_bench_summary(&zk_summary);

    if let Ok(output_path) = std::env::var("ZK_BENCH_OUTPUT") {
        match zk_summary.to_json() {
            Ok(json) => match std::fs::write(&output_path, &json) {
                Ok(()) => println!("ZK bench results written to {output_path}"),
                Err(e) => {
                    eprintln!("Warning: failed to write ZK bench results to {output_path}: {e}")
                }
            },
            Err(e) => eprintln!("Warning: failed to serialize ZK bench results: {e}"),
        }
    }

    Ok(())
}

fn print_cleanup_warnings(cleanup: &LoadTestCleanupSummary) {
    if let Some(error) = &cleanup.b20_teardown_error {
        eprintln!("Warning: B-20 teardown failed: {error}");
    }
    if let Some(error) = &cleanup.drain_error {
        eprintln!("Warning: drain failed: {error}");
    }
}

fn print_zk_bench_summary(summary: &ZkBenchSummary) {
    println!("Target: block {} ({})", summary.target.block, summary.target.reason);
    println!(
        "Proof: session={} protocol_parent={} l1_head={} duration={:.2?}",
        summary.proof.session_id,
        summary.proof.start_block_number,
        summary.proof.l1_head,
        summary.proof.proof_duration
    );

    if let Some(stats) = &summary.execution_stats {
        println!(
            "Execution: cycles={} sp1_gas={} witness_ms={} execution_ms={}",
            stats.total_instruction_cycles,
            stats.total_sp1_gas,
            stats.witness_generation_ms,
            stats.execution_ms
        );
    }
}
