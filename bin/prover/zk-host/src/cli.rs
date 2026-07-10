//! CLI definition for the ZK prover host worker binary.

use std::{collections::HashMap, sync::Arc, time::Duration};

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
    ZkBackend, ZkHost, ZkHostConfig, ZkProver,
};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use clap::Parser;
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

    /// Enable the mock backend. Intended only for tests and local smoke checks.
    #[arg(long, env = "ENABLE_MOCK_ZK_BACKEND", default_value_t = false)]
    enable_mock_zk_backend: bool,

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
    fn optional_string(value: &Option<String>) -> Option<String> {
        value.as_deref().map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned)
    }

    fn rpc_config(&self) -> eyre::Result<Option<SuccinctRpcConfig>> {
        match (
            &self.base_consensus_address,
            &self.l1_node_address,
            &self.l1_beacon_address,
            &self.l2_node_address,
        ) {
            (None, None, None, None) => Ok(None),
            (Some(base_consensus_rpc), Some(l1_rpc), Some(l1_beacon_rpc), Some(l2_rpc)) => {
                Ok(Some(SuccinctRpcConfig {
                    base_consensus_rpc: base_consensus_rpc.clone(),
                    l1_rpc: l1_rpc.clone(),
                    l1_beacon_rpc: l1_beacon_rpc.clone(),
                    l2_rpc: l2_rpc.clone(),
                    default_sequence_window: self.default_sequence_window,
                }))
            }
            _ => Err(eyre!(
                "BASE_CONSENSUS_ADDRESS, L1_NODE_ADDRESS, L1_BEACON_ADDRESS, and L2_NODE_ADDRESS must all be set to enable dry-run, cluster, or network backends"
            )),
        }
    }

    fn duration_from_hours(hours: u64, env: &'static str) -> eyre::Result<Duration> {
        let seconds = hours.checked_mul(3600).ok_or_else(|| eyre!("{env} is too large"))?;
        Ok(Duration::from_secs(seconds))
    }

    fn backend_configs(&self) -> eyre::Result<Vec<(ZkBackend, SuccinctZkBackendConfig)>> {
        let mut configs = Vec::new();
        if self.enable_mock_zk_backend {
            configs.push((ZkBackend::Mock, SuccinctZkBackendConfig::Mock));
        }
        let rpc = self.rpc_config()?;

        if let Some(rpc) = rpc.clone() {
            configs.push((
                ZkBackend::DryRun,
                SuccinctZkBackendConfig::DryRun { rpc, range_cycle_limit: self.range_cycle_limit },
            ));
        }

        if let Some(cluster_rpc) = Self::optional_string(&self.sp1_cluster_api_endpoint) {
            let Some(rpc) = rpc.clone() else {
                return Err(eyre!("cluster backend requires all RPC URLs"));
            };
            let s3_bucket = Self::optional_string(&self.cli_s3_bucket)
                .ok_or_else(|| eyre!("cluster backend requires CLI_S3_BUCKET"))?;
            let s3_region = Self::optional_string(&self.cli_s3_region)
                .ok_or_else(|| eyre!("cluster backend requires CLI_S3_REGION"))?;
            configs.push((
                ZkBackend::Cluster,
                SuccinctZkBackendConfig::Cluster(SuccinctClusterBackendConfig {
                    rpc,
                    cluster_rpc,
                    s3_bucket,
                    s3_region,
                    timeout: Self::duration_from_hours(
                        self.sp1_cluster_timeout_hours,
                        "SP1_CLUSTER_TIMEOUT_HOURS",
                    )?,
                    range_cycle_limit: self.range_cycle_limit,
                    range_gas_limit: self.range_gas_limit,
                    aggregation_cycle_limit: self.aggregation_cycle_limit,
                    aggregation_gas_limit: self.aggregation_gas_limit,
                }),
            ));
        }

        let network_private_key = Self::optional_string(&self.network_private_key);
        if self.use_kms_requester && network_private_key.is_none() {
            return Err(eyre!("USE_KMS_REQUESTER requires NETWORK_PRIVATE_KEY"));
        }
        match (network_private_key, rpc) {
            (Some(network_private_key), Some(rpc)) => {
                configs.push((
                    ZkBackend::Network,
                    SuccinctZkBackendConfig::Network(SuccinctNetworkBackendConfig {
                        rpc,
                        network_private_key,
                        use_kms_requester: self.use_kms_requester,
                        timeout: Self::duration_from_hours(
                            self.sp1_network_timeout_hours,
                            "SP1_NETWORK_TIMEOUT_HOURS",
                        )?,
                        range_cycle_limit: self.range_cycle_limit,
                        range_gas_limit: self.range_gas_limit,
                        aggregation_cycle_limit: self.aggregation_cycle_limit,
                        aggregation_gas_limit: self.aggregation_gas_limit,
                    }),
                ));
            }
            (None, _) => {}
            (Some(_), None) => {
                return Err(eyre!("network backend requires NETWORK_PRIVATE_KEY and all RPC URLs"));
            }
        }

        Ok(configs)
    }

    async fn build_provers(
        &self,
        cancel: &CancellationToken,
    ) -> eyre::Result<Option<HashMap<ZkBackend, Arc<dyn ZkProver>>>> {
        let mut provers = HashMap::new();
        let configs = self.backend_configs()?;
        let witness_provider = if configs.iter().any(|(backend, _)| *backend != ZkBackend::Mock) {
            let Some(rpc) = self.rpc_config()? else {
                return Err(eyre!("non-mock backend requires RPC configuration"));
            };
            let Some(provider) =
                SuccinctZkProverBuilder::build_witness_provider(rpc, cancel).await?
            else {
                return Ok(None);
            };
            Some(provider)
        } else {
            None
        };

        for (backend, config) in configs {
            let mut builder = SuccinctZkProverBuilder::new(config);
            if let Some(provider) = &witness_provider {
                builder = builder.with_witness_provider(provider.clone());
            }
            let Some(prover) = builder
                .build_until_cancelled(cancel)
                .await
                .wrap_err_with(|| format!("failed to initialize {backend:?} zk proving backend"))?
            else {
                return Ok(None);
            };
            provers.insert(backend, prover);
        }

        if provers.is_empty() {
            return Err(eyre!(
                "no ZK backend enabled; configure RPC URLs or explicitly enable the mock backend"
            ));
        }

        Ok(Some(provers))
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

        let Some(provers) = args.build_provers(&cancel).await? else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> WorkerArgs {
        let mut args =
            Cli::try_parse_from(["zk-host", "--prover-service-endpoint", "http://prover-service"])
                .unwrap()
                .worker;
        args.base_consensus_address = None;
        args.l1_node_address = None;
        args.l1_beacon_address = None;
        args.l2_node_address = None;
        args.sp1_cluster_api_endpoint = None;
        args.network_private_key = None;
        args.enable_mock_zk_backend = false;
        args.use_kms_requester = false;
        args
    }

    fn set_rpc_config(args: &mut WorkerArgs) {
        args.base_consensus_address = Some(Url::parse("http://base-consensus").unwrap());
        args.l1_node_address = Some(Url::parse("http://l1").unwrap());
        args.l1_beacon_address = Some(Url::parse("http://l1-beacon").unwrap());
        args.l2_node_address = Some(Url::parse("http://l2").unwrap());
    }

    #[test]
    fn backend_enablement_is_presence_based() {
        let mut args = args();
        assert!(args.backend_configs().unwrap().is_empty());

        args.enable_mock_zk_backend = true;
        let configs = args.backend_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, ZkBackend::Mock);

        args.enable_mock_zk_backend = false;
        set_rpc_config(&mut args);
        let configs = args.backend_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, ZkBackend::DryRun);

        args.base_consensus_address = None;
        assert!(args.backend_configs().is_err());
    }
}
