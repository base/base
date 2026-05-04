use std::path::PathBuf;

use clap::Parser;
use tracing::error;

#[derive(Debug, Parser)]
#[command(name = "base-bench", about = "Base EL benchmark orchestrator")]
struct Cli {
    #[arg(long, env = "BASE_BENCH_CONFIG")]
    config: PathBuf,

    #[arg(long, env = "BASE_BENCH_ROOT_DIR")]
    root_dir: PathBuf,

    #[arg(long, env = "BASE_BENCH_OUTPUT_DIR")]
    output_dir: PathBuf,

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

    tracing::info!(
        config = %cli.config.display(),
        reth_bin = %reth_bin.display(),
        builder_bin = %builder_bin.display(),
        load_test_bin = %load_test_bin.display(),
        "starting base-bench",
    );

    error!("benchmark runner not yet implemented");
    std::process::exit(1);
}
