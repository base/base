//! CLI definition for the ZK prover host worker binary.

use std::time::Duration;

use base_cli_utils::{LogConfig, RuntimeManager};
use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
};
use base_proof_zk_backend::SuccinctZkProversConfig;
use base_proof_zk_host::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES, ProofGeneratorHeartbeatConfig,
    ZkBackend, ZkHost, ZkHostConfig,
};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use clap::Parser;
use eyre::WrapErr;
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;
use uuid::Uuid;

base_cli_utils::define_log_args!("BASE_PROVER_ZK_HOST");
base_cli_utils::define_metrics_args!("BASE_PROVER_ZK_HOST", 7303);

/// ZK prover host worker binary.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(flatten)]
    worker: WorkerArgs,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// Worker-mode arguments for claiming and generating ZK proof jobs.
#[derive(Parser)]
struct WorkerArgs {
    /// Prover-service JSON-RPC endpoint.
    #[arg(long, env = "PROVER_SERVICE_ENDPOINT")]
    prover_service_endpoint: String,

    /// Prover-service JSON-RPC request timeout in seconds.
    #[arg(long, env = "PROVER_SERVICE_REQUEST_TIMEOUT_SECS", default_value_t = 60)]
    prover_service_request_timeout_secs: u64,

    /// Worker identifier used when claiming prover-service jobs.
    #[arg(long, env = "PROVER_WORKER_ID")]
    worker_id: Option<String>,

    /// Base consensus node RPC URL. Required for dry-run, cluster, or network backends.
    #[arg(long, env = "BASE_CONSENSUS_ADDRESS")]
    base_consensus_address: Option<Url>,

    /// L1 execution node RPC URL. Required for dry-run, cluster, or network backends.
    #[arg(long, env = "L1_NODE_ADDRESS")]
    l1_node_address: Option<Url>,

    /// L1 beacon node RPC URL. Required for dry-run, cluster, or network backends.
    #[arg(long, env = "L1_BEACON_ADDRESS")]
    l1_beacon_address: Option<Url>,

    /// L2 execution node RPC URL. Required for dry-run, cluster, or network backends.
    #[arg(long, env = "L2_NODE_ADDRESS")]
    l2_node_address: Option<Url>,

    /// Default sequence window for L1 head calculations.
    #[arg(long, env = "DEFAULT_SEQUENCE_WINDOW", default_value_t = 50)]
    default_sequence_window: u64,

    /// SP1 cluster gRPC endpoint. Enables the cluster backend when set with S3 settings.
    #[arg(long, env = "SP1_CLUSTER_API_ENDPOINT")]
    sp1_cluster_api_endpoint: Option<String>,

    /// SP1 cluster proof timeout in hours.
    #[arg(long, env = "SP1_CLUSTER_TIMEOUT_HOURS", default_value_t = 24)]
    sp1_cluster_timeout_hours: u64,

    /// S3 artifact store bucket for the cluster backend.
    #[arg(long, env = "CLI_S3_BUCKET")]
    cli_s3_bucket: Option<String>,

    /// S3 artifact store region for the cluster backend.
    #[arg(long, env = "CLI_S3_REGION")]
    cli_s3_region: Option<String>,

    /// SP1 network requester private key, or KMS key ARN when `USE_KMS_REQUESTER=true`.
    /// Enables the network backend when set.
    #[arg(long, env = "NETWORK_PRIVATE_KEY", hide_env_values = true)]
    network_private_key: Option<String>,

    /// Use the requester key as an AWS KMS ARN instead of a local private key.
    #[arg(long, env = "USE_KMS_REQUESTER", default_value_t = false)]
    use_kms_requester: bool,

    /// SP1 network proof timeout in hours.
    #[arg(long, env = "SP1_NETWORK_TIMEOUT_HOURS", default_value_t = 24)]
    sp1_network_timeout_hours: u64,

    /// Cycle limit for range proof requests.
    #[arg(long, env = "RANGE_CYCLE_LIMIT", default_value_t = 1_000_000_000_000)]
    range_cycle_limit: u64,

    /// Gas limit for range proof requests.
    #[arg(long, env = "RANGE_GAS_LIMIT", default_value_t = 1_000_000_000_000)]
    range_gas_limit: u64,

    /// Cycle limit for aggregation proof requests.
    #[arg(long, env = "AGGREGATION_CYCLE_LIMIT", default_value_t = 1_000_000_000_000)]
    aggregation_cycle_limit: u64,

    /// Gas limit for aggregation proof requests.
    #[arg(long, env = "AGGREGATION_GAS_LIMIT", default_value_t = 1_000_000_000_000)]
    aggregation_gas_limit: u64,

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

    /// Delay between worker API heartbeats while a proof is being generated.
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

impl WorkerArgs {
    fn backend_config(&self) -> SuccinctZkProversConfig {
        SuccinctZkProversConfig {
            base_consensus_rpc: self.base_consensus_address.clone(),
            l1_rpc: self.l1_node_address.clone(),
            l1_beacon_rpc: self.l1_beacon_address.clone(),
            l2_rpc: self.l2_node_address.clone(),
            default_sequence_window: self.default_sequence_window,
            cluster_rpc: self.sp1_cluster_api_endpoint.clone(),
            cluster_timeout_hours: self.sp1_cluster_timeout_hours,
            s3_bucket: self.cli_s3_bucket.clone(),
            s3_region: self.cli_s3_region.clone(),
            network_private_key: self.network_private_key.clone(),
            use_kms_requester: self.use_kms_requester,
            network_timeout_hours: self.sp1_network_timeout_hours,
            range_cycle_limit: self.range_cycle_limit,
            range_gas_limit: self.range_gas_limit,
            aggregation_cycle_limit: self.aggregation_cycle_limit,
            aggregation_gas_limit: self.aggregation_gas_limit,
        }
    }
}

impl Cli {
    /// Run the worker.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { worker, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        RuntimeManager::new()
            .with_thread_stack_size(8 * 1024 * 1024)
            .run_until_shutdown(|cancel| async move { worker.run(cancel).await })
    }
}

impl WorkerArgs {
    async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        let args = &self;
        info!(
            prover_service_endpoint = %args.prover_service_endpoint,
            "initializing zk prover host worker"
        );

        let Some(provers) = args
            .backend_config()
            .build_until_cancelled(&cancel)
            .await
            .wrap_err("failed to initialize zk proving backends")?
        else {
            info!("zk prover host worker initialization cancelled");
            return Ok(());
        };
        let mut backends: Vec<ZkBackend> = provers.keys().copied().collect();
        backends.sort_unstable_by_key(|backend| backend.as_str());

        let client_config = ProverServiceClientConfig::new(args.prover_service_endpoint.clone())
            .with_request_timeout(Duration::from_secs(args.prover_service_request_timeout_secs));
        let Some(client) = Self::connect_prover_service_client(&client_config, &cancel).await?
        else {
            info!("zk prover host worker startup cancelled");
            return Ok(());
        };

        let heartbeat = ProofGeneratorHeartbeatConfig::with_max_consecutive_failures(
            Duration::from_secs(args.proof_generator_heartbeat_interval_secs),
            args.proof_generator_heartbeat_lock_duration_seconds,
            args.proof_generator_max_consecutive_heartbeat_failures,
        );

        let worker_id =
            args.worker_id.clone().unwrap_or_else(|| format!("zk-host-{}", Uuid::new_v4()));
        let host_config = ZkHostConfig::sp1(worker_id.clone())
            .with_job_discovery_poll_interval(Duration::from_millis(
                args.job_discovery_poll_interval_ms,
            ))
            .with_job_discovery_lock_duration_seconds(args.job_discovery_lock_duration_seconds)
            .with_job_discovery_max_concurrent_jobs(args.job_discovery_max_concurrent_jobs)
            .with_proof_generator_heartbeat(heartbeat);
        let host = ZkHost::new(client, provers, host_config);

        info!(
            worker_id = %worker_id,
            prover_service_endpoint = %args.prover_service_endpoint,
            ?backends,
            "starting zk prover host worker"
        );
        host.run_until_cancelled(cancel).await;
        Ok(())
    }

    async fn connect_prover_service_client(
        client_config: &ProverServiceClientConfig,
        cancel: &CancellationToken,
    ) -> eyre::Result<Option<ProverWorkerClient>> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(None),
            result = async {
                ProverWorkerClient::connect(client_config)
                    .wrap_err("failed to connect to prover service")
            } => result.map(Some),
        }
    }
}
