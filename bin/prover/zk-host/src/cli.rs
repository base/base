//! CLI definition for the ZK prover-service worker host binary.

use std::{fmt, sync::Arc, time::Duration};

use base_cli_utils::{LogConfig, RuntimeManager};
use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
};
use base_proof_zk_backend::{DryRunZkProver, MockZkProver};
use base_proof_zk_host::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES, ProofGeneratorHeartbeatConfig,
    ZkHost, ZkHostConfig, ZkProofClaimType, ZkProver,
};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use clap::{Parser, ValueEnum};
use eyre::WrapErr;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

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
    #[arg(long, env = "PROOF_TYPE", value_enum, default_value = "compressed")]
    proof_type: ZkProofTypeArg,

    /// Proving backend to run.
    #[arg(long, env = "ZK_BACKEND", value_enum)]
    backend: ZkBackendArg,

    /// Delay after an empty or failed discovery attempt, in milliseconds.
    #[arg(long, env = "JOB_DISCOVERY_POLL_INTERVAL_MS", default_value_t = 5_000)]
    job_discovery_poll_interval_ms: u64,

    /// Requested claim lock duration in seconds. Zero uses the server default.
    #[arg(
        long,
        env = "JOB_DISCOVERY_LOCK_DURATION_SECONDS",
        default_value_t = DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS
    )]
    job_discovery_lock_duration_seconds: u32,

    /// Maximum number of claimed proof jobs generated concurrently.
    #[arg(
        long,
        env = "JOB_DISCOVERY_MAX_CONCURRENT_JOBS",
        default_value_t = DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS
    )]
    job_discovery_max_concurrent_jobs: usize,

    /// Delay between worker API heartbeats while a ZK proof is being generated.
    #[arg(long, env = "PROOF_GENERATOR_HEARTBEAT_INTERVAL_SECS", default_value_t = 30)]
    proof_generator_heartbeat_interval_secs: u64,

    /// Requested heartbeat lock duration in seconds. Zero uses the server default.
    #[arg(
        long,
        env = "PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS",
        default_value_t = DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS
    )]
    proof_generator_heartbeat_lock_duration_seconds: u32,

    /// Maximum consecutive retryable heartbeat failures before aborting generation.
    #[arg(
        long,
        env = "PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES",
        default_value_t = DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES
    )]
    proof_generator_max_consecutive_heartbeat_failures: u32,
}

/// ZK proof type argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ZkProofTypeArg {
    /// Claim compressed ZK proofs.
    Compressed,
    /// Claim SNARK Groth16 proofs.
    #[value(alias = "snark_groth16", alias = "groth16")]
    SnarkGroth16,
}

impl From<ZkProofTypeArg> for ZkProofClaimType {
    fn from(proof_type: ZkProofTypeArg) -> Self {
        match proof_type {
            ZkProofTypeArg::Compressed => Self::Compressed,
            ZkProofTypeArg::SnarkGroth16 => Self::SnarkGroth16,
        }
    }
}

/// ZK proving backend argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ZkBackendArg {
    /// Return placeholder proof bytes without an external backend.
    Mock,
    /// Return empty proof bytes without an external backend.
    #[value(alias = "dry_run", alias = "dryrun")]
    DryRun,
}

impl ZkBackendArg {
    fn prover(self) -> Arc<dyn ZkProver> {
        match self {
            Self::Mock => Arc::new(MockZkProver),
            Self::DryRun => Arc::new(DryRunZkProver),
        }
    }
}

impl fmt::Display for ZkBackendArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mock => f.write_str("mock"),
            Self::DryRun => f.write_str("dry_run"),
        }
    }
}

impl Cli {
    /// Run the ZK worker host.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { args, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        RuntimeManager::new()
            .with_thread_stack_size(8 * 1024 * 1024)
            .run_until_shutdown(|cancel| async move { args.run(cancel).await })
    }
}

impl ZkHostArgs {
    async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        let worker_id = self.worker_id.unwrap_or_else(|| format!("zk-host-{}", Uuid::new_v4()));
        let proof_type = ZkProofClaimType::from(self.proof_type);
        let prover = self.backend.prover();

        let client_config = ProverServiceClientConfig::new(self.prover_service_endpoint.clone())
            .with_request_timeout(Duration::from_secs(self.prover_service_request_timeout_secs));
        let client = ProverWorkerClient::connect(&client_config)
            .wrap_err("failed to connect to prover service")?;

        let heartbeat = ProofGeneratorHeartbeatConfig::with_max_consecutive_failures(
            Duration::from_secs(self.proof_generator_heartbeat_interval_secs),
            self.proof_generator_heartbeat_lock_duration_seconds,
            self.proof_generator_max_consecutive_heartbeat_failures,
        );
        let host_config = ZkHostConfig::sp1(worker_id.clone(), proof_type)
            .with_job_discovery_poll_interval(Duration::from_millis(
                self.job_discovery_poll_interval_ms,
            ))
            .with_job_discovery_lock_duration_seconds(self.job_discovery_lock_duration_seconds)
            .with_job_discovery_max_concurrent_jobs(self.job_discovery_max_concurrent_jobs)
            .with_proof_generator_heartbeat(heartbeat);
        let host = ZkHost::new(client, prover, host_config);

        info!(
            worker_id = %worker_id,
            prover_service_endpoint = %self.prover_service_endpoint,
            proof_type = ?proof_type,
            backend = %self.backend,
            "starting zk prover host worker"
        );
        host.run_until_cancelled(cancel).await;

        Ok(())
    }
}
