//! CLI definition for the ZK prover host worker binary.

use std::{fmt, time::Duration};

use base_cli_utils::{LogConfig, RuntimeManager};
use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
};
use base_proof_zk_backend::{
    SuccinctClusterBackendConfig, SuccinctNetworkBackendConfig, SuccinctRpcConfig,
    SuccinctZkBackendConfig, SuccinctZkProverBuilder,
};
use base_proof_zk_host::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES, ProofGeneratorHeartbeatConfig,
    ZkHost, ZkHostConfig,
};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use clap::{Parser, ValueEnum};
use eyre::{WrapErr, eyre};
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

    /// Proving backend to run: `mock`, `dry-run`, `cluster`, or `network`.
    #[arg(long, env = "ZK_BACKEND", value_enum)]
    backend: ZkBackendArg,

    /// Base consensus node RPC URL. Required for `ZK_BACKEND=dry-run`, `cluster`, or `network`.
    #[arg(
        long,
        env = "BASE_CONSENSUS_ADDRESS",
        required_if_eq_any([("backend", "dry-run"), ("backend", "cluster"), ("backend", "network")])
    )]
    base_consensus_address: Option<Url>,

    /// L1 execution node RPC URL. Required for `ZK_BACKEND=dry-run`, `cluster`, or `network`.
    #[arg(
        long,
        env = "L1_NODE_ADDRESS",
        required_if_eq_any([("backend", "dry-run"), ("backend", "cluster"), ("backend", "network")])
    )]
    l1_node_address: Option<Url>,

    /// L1 beacon node RPC URL. Required for `ZK_BACKEND=dry-run`, `cluster`, or `network`.
    #[arg(
        long,
        env = "L1_BEACON_ADDRESS",
        required_if_eq_any([("backend", "dry-run"), ("backend", "cluster"), ("backend", "network")])
    )]
    l1_beacon_address: Option<Url>,

    /// L2 execution node RPC URL. Required for `ZK_BACKEND=dry-run`, `cluster`, or `network`.
    #[arg(
        long,
        env = "L2_NODE_ADDRESS",
        required_if_eq_any([("backend", "dry-run"), ("backend", "cluster"), ("backend", "network")])
    )]
    l2_node_address: Option<Url>,

    /// Default sequence window for L1 head calculations.
    #[arg(long, env = "DEFAULT_SEQUENCE_WINDOW", default_value_t = 50)]
    default_sequence_window: u64,

    /// SP1 cluster gRPC endpoint. Required for `ZK_BACKEND=cluster`.
    #[arg(long, env = "SP1_CLUSTER_API_ENDPOINT", required_if_eq("backend", "cluster"))]
    sp1_cluster_api_endpoint: Option<String>,

    /// SP1 cluster proof timeout in hours.
    #[arg(long, env = "SP1_CLUSTER_TIMEOUT_HOURS", default_value_t = 24)]
    sp1_cluster_timeout_hours: u64,

    /// S3 artifact store bucket for `ZK_BACKEND=cluster`.
    #[arg(long, env = "CLI_S3_BUCKET", required_if_eq("backend", "cluster"))]
    cli_s3_bucket: Option<String>,

    /// S3 artifact store region for `ZK_BACKEND=cluster`.
    #[arg(long, env = "CLI_S3_REGION", required_if_eq("backend", "cluster"))]
    cli_s3_region: Option<String>,

    /// SP1 network requester private key, or KMS key ARN when `USE_KMS_REQUESTER=true`.
    #[arg(
        long,
        env = "NETWORK_PRIVATE_KEY",
        hide_env_values = true,
        required_if_eq("backend", "network")
    )]
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

/// ZK proving backend argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ZkBackendArg {
    /// Return placeholder proof bytes without an external backend.
    Mock,
    /// Run local SP1 execution and return dry-run stats without proof bytes.
    #[value(alias = "dry_run", alias = "dryrun")]
    DryRun,
    /// Submit proofs to an SP1 cluster.
    Cluster,
    /// Submit proofs to the Succinct SP1 Network.
    Network,
}

impl AsRef<str> for ZkBackendArg {
    fn as_ref(&self) -> &str {
        match self {
            Self::Mock => "mock",
            Self::DryRun => "dry_run",
            Self::Cluster => "cluster",
            Self::Network => "network",
        }
    }
}

impl fmt::Display for ZkBackendArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl WorkerArgs {
    fn backend_config(&self) -> eyre::Result<SuccinctZkBackendConfig> {
        match self.backend {
            ZkBackendArg::Mock => Ok(SuccinctZkBackendConfig::Mock),
            ZkBackendArg::DryRun => Ok(SuccinctZkBackendConfig::DryRun {
                rpc: self.rpc_config()?,
                range_cycle_limit: self.range_cycle_limit,
            }),
            ZkBackendArg::Cluster => {
                Ok(SuccinctZkBackendConfig::Cluster(SuccinctClusterBackendConfig {
                    rpc: self.rpc_config()?,
                    cluster_rpc: Self::required_string(
                        &self.sp1_cluster_api_endpoint,
                        "SP1_CLUSTER_API_ENDPOINT",
                    )?,
                    s3_bucket: Self::required_string(&self.cli_s3_bucket, "CLI_S3_BUCKET")?,
                    s3_region: Self::required_string(&self.cli_s3_region, "CLI_S3_REGION")?,
                    timeout: Self::duration_from_hours(
                        self.sp1_cluster_timeout_hours,
                        "SP1_CLUSTER_TIMEOUT_HOURS",
                    )?,
                    range_cycle_limit: self.range_cycle_limit,
                    range_gas_limit: self.range_gas_limit,
                    aggregation_cycle_limit: self.aggregation_cycle_limit,
                    aggregation_gas_limit: self.aggregation_gas_limit,
                }))
            }
            ZkBackendArg::Network => {
                Ok(SuccinctZkBackendConfig::Network(SuccinctNetworkBackendConfig {
                    rpc: self.rpc_config()?,
                    network_private_key: Self::required_string(
                        &self.network_private_key,
                        "NETWORK_PRIVATE_KEY",
                    )?,
                    use_kms_requester: self.use_kms_requester,
                    timeout: Self::duration_from_hours(
                        self.sp1_network_timeout_hours,
                        "SP1_NETWORK_TIMEOUT_HOURS",
                    )?,
                    range_cycle_limit: self.range_cycle_limit,
                    range_gas_limit: self.range_gas_limit,
                    aggregation_cycle_limit: self.aggregation_cycle_limit,
                    aggregation_gas_limit: self.aggregation_gas_limit,
                }))
            }
        }
    }

    fn rpc_config(&self) -> eyre::Result<SuccinctRpcConfig> {
        Ok(SuccinctRpcConfig {
            base_consensus_rpc: Self::required_url(
                &self.base_consensus_address,
                "BASE_CONSENSUS_ADDRESS",
            )?,
            l1_rpc: Self::required_url(&self.l1_node_address, "L1_NODE_ADDRESS")?,
            l1_beacon_rpc: Self::required_url(&self.l1_beacon_address, "L1_BEACON_ADDRESS")?,
            l2_rpc: Self::required_url(&self.l2_node_address, "L2_NODE_ADDRESS")?,
            default_sequence_window: self.default_sequence_window,
        })
    }

    fn required_url(value: &Option<Url>, env: &'static str) -> eyre::Result<Url> {
        value.clone().ok_or_else(|| eyre!("{env} must be set for the selected ZK_BACKEND"))
    }

    fn required_string(value: &Option<String>, env: &'static str) -> eyre::Result<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| eyre!("{env} must be set for the selected ZK_BACKEND"))
    }

    fn duration_from_hours(hours: u64, env: &'static str) -> eyre::Result<Duration> {
        let seconds = hours.checked_mul(3600).ok_or_else(|| eyre!("{env} is too large"))?;
        Ok(Duration::from_secs(seconds))
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
            backend = %args.backend,
            "initializing zk prover host worker"
        );

        let backend_config = args.backend_config()?;
        let Some(prover) = SuccinctZkProverBuilder::new(backend_config)
            .build_until_cancelled(&cancel)
            .await
            .wrap_err("failed to initialize zk proving backend")?
        else {
            info!(
                backend = %args.backend,
                "zk prover host worker initialization cancelled"
            );
            return Ok(());
        };

        let client_config = ProverServiceClientConfig::new(args.prover_service_endpoint.clone())
            .with_request_timeout(Duration::from_secs(args.prover_service_request_timeout_secs));
        let Some(client) = Self::connect_prover_service_client(&client_config, &cancel).await?
        else {
            info!(
                backend = %args.backend,
                "zk prover host worker startup cancelled"
            );
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
        let host = ZkHost::new(client, prover, host_config);

        info!(
            worker_id = %worker_id,
            prover_service_endpoint = %args.prover_service_endpoint,
            backend = %args.backend,
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
