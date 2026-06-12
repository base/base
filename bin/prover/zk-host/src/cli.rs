//! CLI definition for the ZK prover-service worker host binary.

use base_cli_utils::{LogConfig, RuntimeManager};
use clap::{Parser, ValueEnum};
use eyre::eyre;
use tracing::info;

base_cli_utils::define_log_args!("BASE_PROVER_ZK_HOST");
base_cli_utils::define_metrics_args!("BASE_PROVER_ZK_HOST", 7303);

/// ZK prover-service worker host binary.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: ZkHostArgs,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// ZK worker host configuration.
#[derive(Parser, Debug)]
struct ZkHostArgs {
    /// Prover-service JSON-RPC endpoint.
    #[arg(long, env = "PROVER_SERVICE_ENDPOINT")]
    prover_service_endpoint: String,

    /// Prover-service JSON-RPC request timeout in seconds.
    #[arg(long, env = "PROVER_SERVICE_REQUEST_TIMEOUT_SECS", default_value_t = 60)]
    prover_service_request_timeout_secs: u64,

    /// Worker identifier used when claiming prover-service jobs.
    #[arg(long, env = "PROVER_WORKER_ID")]
    worker_id: Option<String>,

    /// ZK proof type this worker should claim.
    #[arg(long, env = "ZK_PROOF_TYPE", default_value = "compressed")]
    proof_type: ZkProofTypeArg,

    /// ZK virtual machines this worker can execute.
    #[arg(long, env = "ZK_VMS", value_delimiter = ',', default_value = "sp1")]
    zk_vms: Vec<ZkVmArg>,

    /// Delay after an empty or failed discovery attempt, in milliseconds.
    #[arg(long, env = "JOB_DISCOVERY_POLL_INTERVAL_MS", default_value_t = 5_000)]
    job_discovery_poll_interval_ms: u64,

    /// Requested claim lock duration in seconds. Zero uses the server default.
    #[arg(long, env = "JOB_DISCOVERY_LOCK_DURATION_SECONDS", default_value_t = 0)]
    job_discovery_lock_duration_seconds: u32,

    /// Maximum number of claimed proof jobs generated concurrently.
    #[arg(long, env = "JOB_DISCOVERY_MAX_CONCURRENT_JOBS", default_value_t = 1)]
    job_discovery_max_concurrent_jobs: usize,

    /// Delay between worker API heartbeats while a ZK proof is being generated.
    #[arg(long, env = "PROOF_GENERATOR_HEARTBEAT_INTERVAL_SECS", default_value_t = 30)]
    proof_generator_heartbeat_interval_secs: u64,

    /// Requested heartbeat lock duration in seconds. Zero uses the server default.
    #[arg(long, env = "PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS", default_value_t = 0)]
    proof_generator_heartbeat_lock_duration_seconds: u32,
}

/// ZK proof type argument.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ZkProofTypeArg {
    /// Claim compressed ZK proofs.
    Compressed,
    /// Claim SNARK Groth16 proofs.
    SnarkGroth16,
}

impl ZkProofTypeArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Compressed => "compressed",
            Self::SnarkGroth16 => "snark-groth16",
        }
    }
}

/// ZK virtual machine argument.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ZkVmArg {
    /// SP1 ZKVM.
    Sp1,
}

impl Cli {
    /// Run the ZK worker host.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { args, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        RuntimeManager::new().run_until_ctrl_c(async move { args.run().await })
    }
}

impl ZkHostArgs {
    async fn run(self) -> eyre::Result<()> {
        info!(
            prover_service_endpoint = %self.prover_service_endpoint,
            prover_service_request_timeout_secs = self.prover_service_request_timeout_secs,
            worker_id = ?self.worker_id,
            proof_type = %self.proof_type.as_str(),
            zk_vms = ?self.zk_vms,
            job_discovery_poll_interval_ms = self.job_discovery_poll_interval_ms,
            job_discovery_lock_duration_seconds = self.job_discovery_lock_duration_seconds,
            job_discovery_max_concurrent_jobs = self.job_discovery_max_concurrent_jobs,
            proof_generator_heartbeat_interval_secs = self.proof_generator_heartbeat_interval_secs,
            proof_generator_heartbeat_lock_duration_seconds =
                self.proof_generator_heartbeat_lock_duration_seconds,
            "zk worker host configuration loaded"
        );

        Err(eyre!("zk prover-service worker loop is not wired yet"))
    }
}
