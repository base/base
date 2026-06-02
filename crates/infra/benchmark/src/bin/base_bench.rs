//! CLI entry point for the `base-bench` benchmark orchestrator.

use std::path::PathBuf;

use clap::Parser;
use tracing::error;

#[derive(Debug, Parser)]
#[command(
    name = "base-bench",
    about = "Base EL benchmark orchestrator",
    long_about = "Runs a configurable benchmark against a Base EL node. \
                  All arguments are optional: with no arguments, runs the built-in \
                  ERC-20 transfer benchmark on a fresh devnet and writes results to ./results/."
)]
struct Cli {
    /// Path to a benchmark YAML config file.
    /// Defaults to the built-in ERC-20 transfer devnet config.
    #[arg(long, env = "BASE_BENCH_CONFIG")]
    config: Option<PathBuf>,

    /// Root working directory for snapshots and (optionally) results.
    /// Defaults to the current directory.
    #[arg(long, env = "BASE_BENCH_ROOT_DIR", default_value = ".")]
    root_dir: PathBuf,

    /// Directory for writing benchmark results.
    /// Defaults to <root-dir>/results.
    #[arg(long, env = "BASE_BENCH_OUTPUT_DIR")]
    output_dir: Option<PathBuf>,

    #[arg(long, env = "BASE_BENCH_RUN_ID")]
    benchmark_run_id: Option<String>,

    #[arg(long, env = "BASE_BENCH_RETH_BIN")]
    reth_bin: Option<PathBuf>,

    #[arg(long, env = "BASE_BENCH_BUILDER_BIN")]
    builder_bin: Option<PathBuf>,

    #[arg(long, env = "BASE_BENCH_LOAD_TEST_BIN")]
    load_test_bin: Option<PathBuf>,

    #[arg(long, env = "BASE_BENCH_MACHINE_TYPE")]
    machine_type: Option<String>,

    #[arg(long, env = "BASE_BENCH_MACHINE_PROVIDER")]
    machine_provider: Option<String>,

    #[arg(long, env = "BASE_BENCH_MACHINE_REGION")]
    machine_region: Option<String>,

    #[arg(long, env = "BASE_BENCH_FILE_SYSTEM")]
    file_system: Option<String>,
}

impl Cli {
    fn resolve_bin(&self, flag: &Option<PathBuf>, name: &str) -> PathBuf {
        if let Some(path) = flag {
            return path.clone();
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join(name);
            }
        }
        PathBuf::from(name)
    }

    fn reth_bin_path(&self) -> PathBuf {
        self.resolve_bin(&self.reth_bin, "base-reth-node")
    }

    fn builder_bin_path(&self) -> PathBuf {
        self.resolve_bin(&self.builder_bin, "base-builder")
    }

    fn load_test_bin_path(&self) -> PathBuf {
        self.resolve_bin(&self.load_test_bin, "base-load-test")
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let reth_bin = cli.reth_bin_path();
    let builder_bin = cli.builder_bin_path();
    let load_test_bin = cli.load_test_bin_path();

    let (config, config_path) = cli.config.as_ref().map_or_else(
        || {
            let config =
                match base_benchmark::parse_config(base_benchmark::DEFAULT_CONFIG_YAML) {
                    Ok(c) => c,
                    Err(e) => {
                        error!(error = %e, "built-in default config failed to parse");
                        std::process::exit(1);
                    }
                };
            (config, None)
        },
        |path| {
            let yaml = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    error!(path = %path.display(), error = %e, "failed to read config file");
                    std::process::exit(1);
                }
            };
            let config = match base_benchmark::parse_config(&yaml) {
                Ok(c) => c,
                Err(e) => {
                    error!(path = %path.display(), error = %e, "failed to parse config file");
                    std::process::exit(1);
                }
            };
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            (config, Some(canonical))
        },
    );

    let output_dir = cli
        .output_dir
        .clone()
        .unwrap_or_else(|| cli.root_dir.join("results"));

    tracing::info!(
        config = config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<default>".to_string()),
        reth_bin = %reth_bin.display(),
        builder_bin = %builder_bin.display(),
        load_test_bin = %load_test_bin.display(),
        output_dir = %output_dir.display(),
        "starting base-bench",
    );

    let prefund_key = std::env::var("BASE_BENCH_PREFUND_KEY")
        .unwrap_or_else(|_| base_benchmark::PREFUND_KEY.to_string());

    let snapshot_dir = cli.root_dir.join("snapshots");

    let args = base_benchmark::BenchmarkArgs {
        config,
        config_path,
        output_dir,
        reth_bin,
        builder_bin,
        load_test_bin,
        prefund_key,
        snapshot_dir,
    };

    if let Err(e) = base_benchmark::run_benchmark(args).await {
        error!(error = %e, "benchmark failed");
        std::process::exit(1);
    }
}
