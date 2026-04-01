//! CLI definition for the ZK prover binary.

use std::sync::Arc;

use base_cli_utils::{LogConfig, RuntimeManager};
use base_zk_client::prover_service_server::ProverServiceServer as ProtoProverServiceServer;
use base_zk_db::{DatabaseConfig, ProofRequestRepo};
use base_zk_outbox::{DatabaseOutboxReader, OutboxProcessor};
use base_zk_service::{
    ArtifactStorageConfig, BackendConfig, BackendRegistry, ProofRequestManager,
    ProverServiceServer, ProverWorkerPool, ProxyConfigs, RateLimitConfig, StatusPoller,
    build_backend, start_all_proxies,
};
use clap::Parser;
use eyre::eyre;
use tonic::transport::Server;
use tracing::info;

base_cli_utils::define_log_args!("BASE_PROVER_ZK");
base_cli_utils::define_metrics_args!("BASE_PROVER_ZK", 7301);

/// ZK prover service binary.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(flatten)]
    args: ZkArgs,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// ZK prover service for proving Base blocks.
#[derive(Parser, Debug)]
struct ZkArgs {
    #[arg(long, env = "BASE_CONSENSUS_ADDRESS")]
    base_consensus_address: String,

    #[arg(long, env = "L1_NODE_ADDRESS")]
    l1_node_address: String,

    #[arg(long, env = "L1_BEACON_ADDRESS")]
    l1_beacon_address: String,

    #[arg(long, env = "L2_NODE_ADDRESS")]
    l2_node_address: String,

    #[arg(long, env = "DEFAULT_SEQUENCE_WINDOW", default_value_t = 50)]
    default_sequence_window: u64,

    #[arg(long, env = "PROXY_ENABLE", default_value_t = true)]
    proxy_enable: bool,

    #[arg(long, env = "PROXY_L2_PORT", default_value_t = 8545)]
    proxy_l2_port: u16,

    #[arg(long, env = "PROXY_L1_PORT", default_value_t = 8546)]
    proxy_l1_port: u16,

    #[arg(long, env = "PROXY_BEACON_PORT", default_value_t = 8547)]
    proxy_beacon_port: u16,

    #[arg(long, env = "RATE_LIMIT_RPS", default_value_t = 50)]
    rate_limit_rps: u32,

    #[arg(long, env = "RATE_LIMIT_CONCURRENT", default_value_t = 25)]
    rate_limit_concurrent: usize,

    #[arg(long, env = "RATE_LIMIT_QUEUE_TIMEOUT_SECS", default_value_t = 90)]
    rate_limit_queue_timeout_secs: u64,

    #[arg(long, env = "OUTBOX_POLL_INTERVAL_SECS", default_value_t = 5)]
    outbox_poll_interval_secs: u64,

    #[arg(long, env = "OUTBOX_BATCH_SIZE", default_value_t = 10)]
    outbox_batch_size: i64,

    #[arg(long, env = "OUTBOX_MAX_RETRIES", default_value_t = 5)]
    outbox_max_retries: i32,

    #[arg(long, env = "STATUS_POLLER_INTERVAL_SECS", default_value_t = 30)]
    status_poller_interval_secs: u64,

    #[arg(long, env = "STUCK_REQUEST_TIMEOUT_MINS", default_value_t = 10)]
    stuck_request_timeout_mins: i32,

    #[arg(long, env = "PROVER_MODE", default_value = "cluster")]
    prover_mode: String,

    #[arg(long, env = "CLUSTER_API_ENDPOINT")]
    cluster_api_endpoint: Option<String>,

    #[arg(long, env = "CLUSTER_TIMEOUT_HOURS", default_value_t = 24)]
    cluster_timeout_hours: u64,

    #[arg(long, env = "ARTIFACT_REDIS_NODES")]
    artifact_redis_nodes: Option<String>,

    #[arg(long, env = "ARTIFACT_S3_BUCKET")]
    artifact_s3_bucket: Option<String>,

    #[arg(long, env = "ARTIFACT_S3_REGION")]
    artifact_s3_region: Option<String>,

    #[arg(long, env = "ARTIFACT_GCS_BUCKET")]
    artifact_gcs_bucket: Option<String>,

    #[arg(long, env = "ARTIFACT_GCS_CONCURRENCY", default_value_t = 32)]
    artifact_gcs_concurrency: usize,

    #[arg(long, env = "GRPC_LISTEN_ADDR", default_value = "0.0.0.0:9000")]
    grpc_listen_addr: String,
}

impl Cli {
    /// Run the ZK prover service.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { args, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::run_until_ctrl_c(async move { args.run().await })
    }
}

impl ZkArgs {
    /// Runs the ZK prover service.
    async fn run(self) -> eyre::Result<()> {
        self.validate_config()?;

        info!("initializing database connection");
        let db_config = DatabaseConfig::from_env().map_err(|e| eyre!(e))?;
        let pool = db_config.init_pool().await.map_err(|e| eyre!(e))?;
        let repo = ProofRequestRepo::new(pool);
        info!("database connection initialized");

        let (l1_url, l2_url, beacon_url, proxy_handles) = if self.proxy_enable {
            info!("proxy enabled, starting rate-limited RPC proxies");

            let rate_limit = RateLimitConfig {
                requests_per_second: self.rate_limit_rps,
                max_concurrent_requests: self.rate_limit_concurrent,
                queue_timeout_secs: self.rate_limit_queue_timeout_secs,
            };

            let proxy_configs = ProxyConfigs::new(
                self.proxy_l1_port,
                self.l1_node_address.clone(),
                self.proxy_l2_port,
                self.l2_node_address.clone(),
                self.proxy_beacon_port,
                self.l1_beacon_address.clone(),
                rate_limit,
            );

            let handles = start_all_proxies(proxy_configs.clone()).await.map_err(|e| eyre!(e))?;

            (
                proxy_configs.l1.local_address(),
                proxy_configs.l2.local_address(),
                proxy_configs.beacon.local_address(),
                handles,
            )
        } else {
            info!("proxy disabled, using direct node connections");
            (
                self.l1_node_address.clone(),
                self.l2_node_address.clone(),
                self.l1_beacon_address.clone(),
                Vec::new(),
            )
        };

        info!(l1_url = %l1_url, l2_url = %l2_url, beacon_url = %beacon_url, "using RPC URLs");

        let artifact_storage = self.resolve_artifact_storage()?;

        let config = BackendConfig::GenericZkvm {
            base_consensus_url: self.base_consensus_address.clone(),
            l1_node_url: l1_url.clone(),
            l1_beacon_url: beacon_url.clone(),
            l2_node_url: l2_url.clone(),
            default_sequence_window: self.default_sequence_window,
            cluster_rpc: self
                .cluster_api_endpoint
                .clone()
                .ok_or_else(|| eyre!("CLUSTER_API_ENDPOINT is required"))?,
            artifact_storage,
            timeout_hours: self.cluster_timeout_hours,
        };

        let backend = build_backend(config).await.map_err(|e| eyre!(e))?;

        let mut backend_registry = BackendRegistry::new();
        backend_registry.register(backend);
        let backend_registry = Arc::new(backend_registry);

        info!("starting outbox processor");

        let outbox_reader = DatabaseOutboxReader::new(repo.clone(), self.outbox_max_retries);
        let prover_worker_pool = ProverWorkerPool::new(repo.clone(), Arc::clone(&backend_registry));

        let outbox_processor = OutboxProcessor::new(
            outbox_reader,
            prover_worker_pool,
            self.outbox_poll_interval_secs,
            self.outbox_batch_size,
        );

        let outbox_handle = tokio::spawn(async move {
            outbox_processor.run().await;
        });

        let manager = ProofRequestManager::new(repo.clone(), Arc::clone(&backend_registry));

        info!("starting status poller");
        let status_poller = StatusPoller::new(
            repo.clone(),
            manager.clone(),
            self.status_poller_interval_secs,
            self.stuck_request_timeout_mins,
        );
        let status_handle = tokio::spawn(async move {
            status_poller.run().await;
        });

        let prover_server = ProverServiceServer::new(repo.clone());

        let addr = self.grpc_listen_addr.parse()?;

        info!(addr = %addr, "starting ZK prover gRPC service");

        let grpc_handle = async {
            Server::builder()
                .add_service(ProtoProverServiceServer::new(prover_server))
                .serve(addr)
                .await
        };

        let proxy_monitor_handle = tokio::spawn(async move {
            if proxy_handles.is_empty() {
                std::future::pending::<()>().await;
                return;
            }
            let (result, _index, _remaining) = futures::future::select_all(proxy_handles).await;
            match result {
                Ok(()) => tracing::error!("a proxy server exited unexpectedly"),
                Err(e) => tracing::error!(error = %e, "a proxy server panicked"),
            }
        });

        tokio::select! {
            result = outbox_handle => {
                match result {
                    Ok(()) => eyre::bail!("outbox processor exited unexpectedly"),
                    Err(e) => eyre::bail!("outbox processor panicked: {e}"),
                }
            }
            result = status_handle => {
                match result {
                    Ok(()) => eyre::bail!("status poller exited unexpectedly"),
                    Err(e) => eyre::bail!("status poller panicked: {e}"),
                }
            }
            result = grpc_handle => {
                result.map_err(|e| eyre!("gRPC server failed: {e}"))?;
            }
            result = proxy_monitor_handle => {
                match result {
                    Ok(()) => eyre::bail!("proxy server exited unexpectedly"),
                    Err(e) => eyre::bail!("proxy server panicked: {e}"),
                }
            }
        }

        Ok(())
    }

    fn validate_config(&self) -> eyre::Result<()> {
        if self.prover_mode != "cluster" {
            eyre::bail!("PROVER_MODE must be set to 'cluster', got '{}'", self.prover_mode);
        }

        if !non_empty(&self.cluster_api_endpoint) {
            eyre::bail!("CLUSTER_API_ENDPOINT must be set");
        }

        let has_redis = non_empty(&self.artifact_redis_nodes);
        let has_s3 = non_empty(&self.artifact_s3_bucket);
        let has_gcs = non_empty(&self.artifact_gcs_bucket);
        let artifact_store_count = [has_redis, has_s3, has_gcs].iter().filter(|&&x| x).count();

        if artifact_store_count == 0 {
            eyre::bail!(
                "exactly one artifact storage backend must be configured: \
                 ARTIFACT_REDIS_NODES, ARTIFACT_S3_BUCKET, or ARTIFACT_GCS_BUCKET"
            );
        }
        if artifact_store_count > 1 {
            eyre::bail!("only one artifact storage backend can be configured at a time");
        }

        if has_s3 && !non_empty(&self.artifact_s3_region) {
            eyre::bail!("ARTIFACT_S3_REGION must be set when using S3 artifact storage");
        }

        info!("configuration validated");

        Ok(())
    }

    fn resolve_artifact_storage(&self) -> eyre::Result<ArtifactStorageConfig> {
        if non_empty(&self.artifact_redis_nodes) {
            let nodes: Vec<String> = self
                .artifact_redis_nodes
                .as_ref()
                .ok_or_else(|| eyre!("ARTIFACT_REDIS_NODES is set but empty"))?
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            info!("using Redis artifact storage");
            Ok(ArtifactStorageConfig::Redis { nodes })
        } else if non_empty(&self.artifact_s3_bucket) {
            let bucket = self
                .artifact_s3_bucket
                .as_ref()
                .ok_or_else(|| eyre!("ARTIFACT_S3_BUCKET is set but empty"))?
                .clone();
            let region = self
                .artifact_s3_region
                .as_ref()
                .ok_or_else(|| eyre!("ARTIFACT_S3_REGION is required for S3 storage"))?
                .clone();
            info!("using S3 artifact storage");
            Ok(ArtifactStorageConfig::S3 { bucket, region })
        } else if non_empty(&self.artifact_gcs_bucket) {
            let bucket = self
                .artifact_gcs_bucket
                .as_ref()
                .ok_or_else(|| eyre!("ARTIFACT_GCS_BUCKET is set but empty"))?
                .clone();
            let concurrency = self.artifact_gcs_concurrency;
            info!("using GCS artifact storage");
            Ok(ArtifactStorageConfig::Gcs { bucket, concurrency })
        } else {
            eyre::bail!(
                "no artifact storage configured; \
                 set ARTIFACT_REDIS_NODES, ARTIFACT_S3_BUCKET, or ARTIFACT_GCS_BUCKET"
            );
        }
    }
}

fn non_empty(opt: &Option<String>) -> bool {
    opt.as_ref().is_some_and(|s| !s.is_empty())
}
