//! CLI definition for the ZK prover binary.

use std::sync::Arc;

use base_cli_utils::{LogConfig, RuntimeManager};
use base_proof_succinct_host_utils::fetcher::RPCConfig;
use base_zk_client::prover_service_server::ProverServiceServer as ProtoProverServiceServer;
use base_zk_db::{DatabaseConfig, ProofRequestRepo};
use base_zk_outbox::{DatabaseOutboxReader, OutboxProcessor};
use base_zk_service::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, MockBackend,
    NetworkBackend, OpSuccinctBackend, OpSuccinctProvider, ProofRequestManager,
    ProverServiceServer, ProverWorkerPool, ProxyConfigs, RateLimitConfig, StatusPoller,
    start_all_proxies,
};
use clap::Parser;
use eyre::eyre;
use http::header;
use tonic::transport::Server;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tracing::info;
use url::Url;

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

    #[arg(long, env = "MAX_PROOF_RETRIES", default_value_t = 3)]
    max_proof_retries: u32,

    #[arg(long, env = "SP1_PROVER", default_value = "cluster")]
    prover_mode: String,

    #[arg(long, env = "SP1_CLUSTER_API_ENDPOINT")]
    sp1_cluster_api_endpoint: Option<String>,

    #[arg(long, env = "SP1_CLUSTER_TIMEOUT_HOURS", default_value_t = 24)]
    sp1_cluster_timeout_hours: u64,

    #[arg(long, env = "CLI_REDIS_NODES")]
    cli_redis_nodes: Option<String>,

    #[arg(long, env = "CLI_S3_BUCKET")]
    cli_s3_bucket: Option<String>,

    #[arg(long, env = "CLI_S3_REGION")]
    cli_s3_region: Option<String>,

    #[arg(long, env = "SP1_NETWORK_PRIVATE_KEY")]
    sp1_network_private_key: Option<String>,

    #[arg(long, env = "SP1_FULFILLMENT_STRATEGY", default_value = "reserved")]
    sp1_fulfillment_strategy: String,

    #[arg(long, env = "USE_KMS_REQUESTER", default_value_t = false)]
    use_kms_requester: bool,

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
            base_zk_service::ProverMetrics::init();
        })?;
        RuntimeManager::new().run_until_ctrl_c(async move { args.run().await })
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

        let rpc_config = RPCConfig {
            l1_rpc: Url::parse(&l1_url).map_err(|e| eyre!("invalid L1 RPC URL: {e}"))?,
            l1_beacon_rpc: Some(
                Url::parse(&beacon_url).map_err(|e| eyre!("invalid beacon RPC URL: {e}"))?,
            ),
            l2_rpc: Url::parse(&l2_url).map_err(|e| eyre!("invalid L2 RPC URL: {e}"))?,
            l2_node_rpc: Url::parse(&self.base_consensus_address)
                .map_err(|e| eyre!("invalid L2 node RPC URL: {e}"))?,
        };

        info!("computing range and aggregation verifying keys");
        let (range_pk, range_vk, agg_pk, agg_vk) =
            base_proof_succinct_proof_utils::cluster_setup_keys()
                .await
                .map_err(|e| eyre!("failed to compute verifying keys: {e}"))?;
        info!("verifying keys computed successfully");

        let mut backend_registry = BackendRegistry::new();

        if self.prover_mode == "mock" {
            info!("SP1_PROVER=mock: using MockBackend (instant fake proofs, no cluster)");
            let mock_backend = MockBackend::new(range_vk, agg_vk);
            backend_registry.register(Arc::new(mock_backend));
        } else if self.prover_mode == "network" {
            info!("SP1_PROVER=network: using OP-Succinct SP1 Network backend");

            let fetcher = Arc::new(
                base_proof_succinct_host_utils::fetcher::OPSuccinctDataFetcher::from_rpc_config_with_rollup_config(rpc_config)
                    .await
                    .map_err(|e| eyre!("failed to create OPSuccinctDataFetcher: {e}"))?,
            );
            let provider = OpSuccinctProvider::new(fetcher);

            let fulfillment_strategy =
                base_proof_succinct_host_utils::network::parse_fulfillment_strategy(
                    self.sp1_fulfillment_strategy.clone(),
                )
                .map_err(|e| eyre!("invalid fulfillment strategy: {e}"))?;

            let network_signer =
                base_proof_succinct_host_utils::network::get_network_signer(self.use_kms_requester)
                    .await
                    .map_err(|e| eyre!("failed to create network signer: {e}"))?;

            let network_mode = match fulfillment_strategy {
                sp1_sdk::network::FulfillmentStrategy::Auction => {
                    sp1_sdk::network::NetworkMode::Mainnet
                }
                _ => sp1_sdk::network::NetworkMode::Reserved,
            };

            info!(
                network_mode = ?network_mode,
                fulfillment_strategy = ?fulfillment_strategy,
                "creating SP1 Network prover"
            );

            let network_prover = Arc::new(
                sp1_sdk::ProverClient::builder()
                    .network_for(network_mode)
                    .signer(network_signer)
                    .build()
                    .await,
            );

            let config = BackendConfig::Network {
                base_consensus_url: self.base_consensus_address.clone(),
                l1_node_url: l1_url.clone(),
                l1_beacon_url: beacon_url.clone(),
                l2_node_url: l2_url.clone(),
                default_sequence_window: self.default_sequence_window,
                network_prover,
                range_pk,
                range_vk,
                agg_pk,
                agg_vk,
                fulfillment_strategy,
                timeout_hours: self.sp1_cluster_timeout_hours,
            };

            let backend = Arc::new(NetworkBackend::new(provider, config));
            backend_registry.register(backend);
        } else {
            info!("SP1_PROVER=cluster: using OP-Succinct cluster backend");

            info!("creating OP-Succinct data fetcher");
            let fetcher = Arc::new(
                base_proof_succinct_host_utils::fetcher::OPSuccinctDataFetcher::from_rpc_config_with_rollup_config(rpc_config)
                    .await
                    .map_err(|e| eyre!("failed to create OPSuccinctDataFetcher: {e}"))?,
            );
            let provider = OpSuccinctProvider::new(fetcher);

            // Create SP1 cluster gRPC client.
            let cluster_rpc = self
                .sp1_cluster_api_endpoint
                .clone()
                .ok_or_else(|| eyre!("SP1_CLUSTER_API_ENDPOINT is required"))?;
            info!(cluster_rpc = %cluster_rpc, "creating SP1 cluster client");
            let cluster_client =
                sp1_cluster_common::client::ClusterServiceClient::new(cluster_rpc.clone())
                    .await
                    .map_err(|e| eyre!("failed to create SP1 cluster client: {e}"))?;

            // Create artifact client and storage config.
            let (artifact_client, artifact_storage_config) = self.create_artifact_client().await?;

            info!("created SP1 cluster client and artifact client");

            let config = BackendConfig::OpSuccinct {
                base_consensus_url: self.base_consensus_address.clone(),
                l1_node_url: l1_url.clone(),
                l1_beacon_url: beacon_url.clone(),
                l2_node_url: l2_url.clone(),
                default_sequence_window: self.default_sequence_window,
                cluster_rpc,
                cluster_client,
                artifact_client,
                artifact_storage_config,
                timeout_hours: self.sp1_cluster_timeout_hours,
                range_vk,
            };

            let backend = Arc::new(OpSuccinctBackend::new(provider, config));
            backend_registry.register(backend);
        }

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
            self.max_proof_retries as i32,
        );
        let status_handle = tokio::spawn(async move {
            status_poller.run().await;
        });

        let prover_server = ProverServiceServer::new(repo.clone(), manager.clone());

        let addr = self.grpc_listen_addr.parse()?;

        info!(addr = %addr, "starting ZK prover gRPC service");

        let reflection_service = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(base_zk_client::PROVER_FILE_DESCRIPTOR_SET)
            .build_v1()
            .map_err(|e| eyre!("failed to build gRPC reflection service: {e}"))?;

        let cors = CorsLayer::new()
            .allow_origin(AllowOrigin::any())
            .allow_headers(AllowHeaders::list([
                header::CONTENT_TYPE,
                "x-grpc-web".parse().expect("valid header name"),
                "x-user-agent".parse().expect("valid header name"),
            ]))
            .allow_methods(AllowMethods::list([http::Method::POST, http::Method::OPTIONS]));

        let grpc_handle = async {
            Server::builder()
                .accept_http1(true)
                .layer(cors)
                .layer(tonic_web::GrpcWebLayer::new())
                .initial_connection_window_size(Some(1024 * 1024))
                .initial_stream_window_size(Some(1024 * 1024))
                .max_frame_size(Some(1024 * 1024))
                .add_service(
                    ProtoProverServiceServer::new(prover_server)
                        .max_decoding_message_size(256 * 1024 * 1024)
                        .max_encoding_message_size(256 * 1024 * 1024),
                )
                .add_service(reflection_service)
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
        if !matches!(self.prover_mode.as_str(), "cluster" | "mock" | "network") {
            eyre::bail!(
                "SP1_PROVER must be set to 'cluster', 'mock', or 'network', got '{}'",
                self.prover_mode
            );
        }

        if self.prover_mode == "mock" {
            info!(prover_mode = "mock", "configuration validated");
            return Ok(());
        }

        if self.prover_mode == "network" {
            if !self.use_kms_requester && !Self::non_empty(&self.sp1_network_private_key) {
                eyre::bail!(
                    "SP1_NETWORK_PRIVATE_KEY must be set (or USE_KMS_REQUESTER=true) for network mode"
                );
            }
            info!(prover_mode = "network", "configuration validated");
            return Ok(());
        }

        if !Self::non_empty(&self.sp1_cluster_api_endpoint) {
            eyre::bail!("SP1_CLUSTER_API_ENDPOINT must be set");
        }

        let has_redis = Self::non_empty(&self.cli_redis_nodes);
        let has_s3 = Self::non_empty(&self.cli_s3_bucket);
        let artifact_store_count = [has_redis, has_s3].iter().filter(|&&x| x).count();

        if artifact_store_count == 0 {
            eyre::bail!(
                "exactly one artifact storage backend must be configured: \
                 CLI_REDIS_NODES or CLI_S3_BUCKET"
            );
        }
        if artifact_store_count > 1 {
            eyre::bail!("only one artifact storage backend can be configured at a time");
        }

        if has_s3 && !Self::non_empty(&self.cli_s3_region) {
            eyre::bail!("CLI_S3_REGION must be set when using S3 artifact storage");
        }

        info!(prover_mode = "cluster", "configuration validated");

        Ok(())
    }

    /// Creates the artifact client and its corresponding storage config descriptor.
    async fn create_artifact_client(
        &self,
    ) -> eyre::Result<(ArtifactClientWrapper, ArtifactStorageConfig)> {
        if Self::non_empty(&self.cli_redis_nodes) {
            let nodes: Vec<String> = self
                .cli_redis_nodes
                .as_ref()
                .ok_or_else(|| eyre!("CLI_REDIS_NODES is set but empty"))?
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            info!("using Redis artifact storage");
            let client = sp1_cluster_artifact::redis::RedisArtifactClient::new(nodes.clone(), 16);
            Ok((ArtifactClientWrapper::Redis(client), ArtifactStorageConfig::Redis { nodes }))
        } else if Self::non_empty(&self.cli_s3_bucket) {
            let bucket = self
                .cli_s3_bucket
                .as_ref()
                .ok_or_else(|| eyre!("CLI_S3_BUCKET is set but empty"))?
                .clone();
            let region = self
                .cli_s3_region
                .as_ref()
                .ok_or_else(|| eyre!("CLI_S3_REGION is required for S3 storage"))?
                .clone();
            info!("using S3 artifact storage");
            let client = sp1_cluster_artifact::s3::S3ArtifactClient::new(
                region.clone(),
                bucket.clone(),
                32,
                sp1_cluster_artifact::s3::S3DownloadMode::AwsSDK(
                    sp1_cluster_artifact::s3::S3ArtifactClient::create_s3_sdk_download_client(
                        region.clone(),
                    )
                    .await,
                ),
            )
            .await;
            Ok((ArtifactClientWrapper::S3(client), ArtifactStorageConfig::S3 { bucket, region }))
        } else {
            eyre::bail!(
                "no artifact storage configured; \
                 set CLI_REDIS_NODES or CLI_S3_BUCKET"
            );
        }
    }

    fn non_empty(opt: &Option<String>) -> bool {
        opt.as_ref().is_some_and(|s| !s.is_empty())
    }
}
