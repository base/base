//! CLI definition for the Intel TDX TEE prover binary.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_chains::rollup_config;
use base_proof_host::ProverConfig;
use base_proof_tee_tdx_prover::{
    DEFAULT_TDX_WORKER_ID, MeasuredMockTdxQuoteProvider, ProofGenerator, TdxEnclaveService,
    TdxProverServer,
};
use base_proof_tee_tdx_runtime::{ConfigfsTdxQuoteProvider, TdxQuoteProvider, TdxRuntime};
use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
    DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS, JobDiscovery, JobDiscoveryConfig,
    ProofSubmitter, WorkerHeartbeatConfig,
};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use base_prover_service_protocol::TeeKind;
use clap::{Parser, Subcommand};
use eyre::eyre;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

base_cli_utils::define_log_args!("BASE_PROVER_TDX");
base_cli_utils::define_metrics_args!("BASE_PROVER_TDX", 7310);

/// Intel TDX TEE prover binary.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// TDX prover subcommands.
#[derive(Subcommand)]
enum Command {
    /// Run a prover-service worker using Linux TSM/configfs quote collection.
    Server(ServerArgs),

    /// Run a prover-service worker with deterministic local TDX quote fixtures.
    Local(LocalArgs),
}

/// Shared arguments for TDX worker modes.
#[derive(Parser)]
struct ProverRuntimeArgs {
    /// L1 execution layer RPC URL.
    #[arg(long, env = "L1_ETH_URL")]
    l1_eth_url: String,

    /// L2 execution layer RPC URL.
    #[arg(long, env = "L2_ETH_URL")]
    l2_eth_url: String,

    /// L1 beacon API URL.
    #[arg(long, env = "L1_BEACON_URL")]
    l1_beacon_url: String,

    /// L2 chain ID.
    #[arg(long, env = "L2_CHAIN_ID")]
    l2_chain_id: u64,

    /// Socket address for the registrar-facing signer JSON-RPC API.
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8000")]
    listen_addr: SocketAddr,

    /// Enable experimental `debug_executePayload` witness endpoint.
    #[arg(long, env = "ENABLE_EXPERIMENTAL_WITNESS_ENDPOINT")]
    enable_experimental_witness_endpoint: bool,
}

impl ProverRuntimeArgs {
    fn into_prover_config(self) -> eyre::Result<ProverConfig> {
        let rollup_config = rollup_config!(self.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.l2_chain_id))?;

        let l1_config = base_common_chains::L1_CONFIGS
            .get(&rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        Ok(ProverConfig {
            l1_eth_url: self.l1_eth_url,
            l2_eth_url: self.l2_eth_url,
            l1_beacon_url: self.l1_beacon_url,
            l2_chain_id: self.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.enable_experimental_witness_endpoint,
        })
    }
}

/// Worker-mode arguments for claiming and generating TDX proof jobs.
#[derive(Parser)]
struct WorkerArgs {
    /// Prover-service JSON-RPC endpoint.
    #[arg(long, env = "PROVER_SERVICE_ENDPOINT")]
    prover_service_endpoint: String,

    /// Prover-service JSON-RPC request timeout in seconds.
    #[arg(long, env = "PROVER_SERVICE_REQUEST_TIMEOUT_SECS", default_value_t = 60)]
    prover_service_request_timeout_secs: u64,

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

    /// Delay between worker API heartbeats while a TDX proof is being generated.
    #[arg(long, env = "PROOF_GENERATOR_HEARTBEAT_INTERVAL_SECS", default_value_t = 30)]
    proof_generator_heartbeat_interval_secs: u64,

    /// Requested heartbeat lock duration in seconds. Zero uses the server default.
    #[arg(
        long,
        env = "PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS",
        default_value_t = DEFAULT_WORKER_HEARTBEAT_LOCK_DURATION_SECONDS
    )]
    proof_generator_heartbeat_lock_duration_seconds: u32,
}

/// Arguments for the TDX configfs worker mode.
#[derive(Parser)]
struct ServerArgs {
    #[command(flatten)]
    runtime: ProverRuntimeArgs,

    #[command(flatten)]
    worker: WorkerArgs,

    /// Configfs report name below `/sys/kernel/config/tsm/report`.
    #[arg(long, env = "TDX_REPORT_NAME", default_value = "base-tdx-prover")]
    report_name: String,
}

/// Arguments for local deterministic mock mode.
#[derive(Parser)]
struct LocalArgs {
    #[command(flatten)]
    runtime: ProverRuntimeArgs,

    #[command(flatten)]
    worker: WorkerArgs,
}

impl Cli {
    /// Run the selected subcommand.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { command, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::new().with_thread_stack_size(8 * 1024 * 1024).run_until_shutdown(
            |cancel| async move {
                match command {
                    Command::Server(args) => args.run(cancel).await,
                    Command::Local(args) => args.run(cancel).await,
                }
            },
        )
    }
}

impl ServerArgs {
    async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        let provider = ConfigfsTdxQuoteProvider::new(&self.report_name);
        info!(
            addr = %self.runtime.listen_addr,
            report_name = %self.report_name,
            "starting tdx prover worker"
        );
        run_worker(self.runtime, self.worker, provider, cancel).await
    }
}

impl LocalArgs {
    async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        let provider = MeasuredMockTdxQuoteProvider::local_mock();
        info!(addr = %self.runtime.listen_addr, "starting tdx prover worker (local mock mode)");
        run_worker(self.runtime, self.worker, provider, cancel).await
    }
}

async fn run_worker(
    runtime_args: ProverRuntimeArgs,
    worker: WorkerArgs,
    provider: impl TdxQuoteProvider + 'static,
    cancel: CancellationToken,
) -> eyre::Result<()> {
    let listen_addr = runtime_args.listen_addr;
    let config = runtime_args.into_prover_config()?;
    let runtime = Arc::new(TdxRuntime::new(provider));
    let registrar_handle = TdxProverServer::new(Arc::clone(&runtime)).run(listen_addr).await?;
    let enclave = TdxEnclaveService::new(config, runtime);

    let prover_service = ProverServiceClientConfig::new(worker.prover_service_endpoint.clone())
        .with_request_timeout(Duration::from_secs(worker.prover_service_request_timeout_secs));
    let client = ProverWorkerClient::connect(&prover_service)?;
    let submitter = ProofSubmitter::new(client.clone());
    let heartbeat = WorkerHeartbeatConfig::new(
        Duration::from_secs(worker.proof_generator_heartbeat_interval_secs),
        worker.proof_generator_heartbeat_lock_duration_seconds,
    );
    let proof_generator = Arc::new(ProofGenerator::new(enclave, submitter, heartbeat));
    let worker_id = format!("{DEFAULT_TDX_WORKER_ID}-{}", Uuid::new_v4());
    let discovery_config = JobDiscoveryConfig::tee(worker_id.clone(), vec![TeeKind::IntelTdx])
        .with_poll_interval(Duration::from_millis(worker.job_discovery_poll_interval_ms))
        .with_lock_duration_seconds(worker.job_discovery_lock_duration_seconds)
        .with_max_concurrent_jobs(worker.job_discovery_max_concurrent_jobs);
    let discovery = JobDiscovery::new(client, proof_generator, discovery_config);

    info!(
        worker_id = %worker_id,
        prover_service_endpoint = %worker.prover_service_endpoint,
        "tdx prover worker started"
    );
    discovery.run_until_cancelled(cancel).await;
    let _ = registrar_handle.stop();
    registrar_handle.stopped().await;
    Ok(())
}
