//! Audit archiver binary entry point.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use audit_archiver_lib::{
    AuditArchiver, AuditArchiverApiServer, AuditArchiverRpc, RpcEventReader, S3EventReaderWriter,
};
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, config::Builder as S3ConfigBuilder};
use base_cli_utils::LogConfig;
use clap::{Parser, ValueEnum};
use jsonrpsee::server::ServerBuilder;
use moka::{policy::EvictionPolicy, sync::Cache};
use tokio::sync::mpsc;
use tracing::{info, warn};

base_cli_utils::define_log_args!("TIPS_AUDIT");
base_cli_utils::define_metrics_args!("TIPS_AUDIT", 9002);

#[derive(Debug, Clone, ValueEnum)]
enum S3ConfigType {
    Aws,
    Manual,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Deprecated: bundle events are now ingested over RPC. Accepted for
    /// backward compatibility with existing deploy configs and ignored at
    /// runtime (a deprecation warning is logged when set).
    #[arg(long, env = "TIPS_AUDIT_KAFKA_PROPERTIES_FILE")]
    kafka_properties_file: Option<String>,

    /// Deprecated: bundle events are now ingested over RPC. Accepted for
    /// backward compatibility with existing deploy configs and ignored at
    /// runtime (a deprecation warning is logged when set).
    #[arg(long, env = "TIPS_AUDIT_KAFKA_TOPIC")]
    kafka_topic: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_S3_BUCKET")]
    s3_bucket: String,

    #[command(flatten)]
    log: LogArgs,

    #[command(flatten)]
    metrics: MetricsArgs,

    #[arg(long, env = "TIPS_AUDIT_S3_CONFIG_TYPE", default_value = "aws")]
    s3_config_type: S3ConfigType,

    #[arg(long, env = "TIPS_AUDIT_S3_ENDPOINT")]
    s3_endpoint: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_S3_REGION", default_value = "us-east-1")]
    s3_region: String,

    #[arg(long, env = "TIPS_AUDIT_S3_ACCESS_KEY_ID")]
    s3_access_key_id: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_S3_SECRET_ACCESS_KEY")]
    s3_secret_access_key: Option<String>,

    #[arg(long, env = "TIPS_AUDIT_WORKER_POOL_SIZE", default_value = "80")]
    worker_pool_size: usize,

    #[arg(long, env = "TIPS_AUDIT_CHANNEL_BUFFER_SIZE", default_value = "1024")]
    channel_buffer_size: usize,

    #[arg(long, env = "TIPS_AUDIT_RPC_PORT", default_value = "9100")]
    rpc_port: u16,

    #[arg(long, env = "TIPS_AUDIT_NOOP_ARCHIVE", default_value = "false")]
    noop_archive: bool,

    /// Maximum number of dedup-cache entries (event-key → ()). Cross-pod dedup
    /// is enforced at the S3 layer; this cache short-circuits in-pod dupes.
    #[arg(long, env = "TIPS_AUDIT_RPC_CACHE_CAPACITY", default_value = "100000")]
    rpc_cache_capacity: u64,

    /// Time-to-live in seconds for entries in the dedup cache.
    #[arg(long, env = "TIPS_AUDIT_RPC_CACHE_TTL_SECS", default_value = "300")]
    rpc_cache_ttl_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args = Args::parse();

    LogConfig::from(args.log.clone())
        .init_tracing_subscriber()
        .expect("Failed to initialize tracing");

    base_cli_utils::MetricsConfig::from(args.metrics.clone())
        .init()
        .expect("Failed to install Prometheus exporter");

    info!(
        s3_bucket = %args.s3_bucket,
        metrics_addr = %args.metrics.addr,
        metrics_port = args.metrics.port,
        rpc_port = args.rpc_port,
        rpc_cache_capacity = args.rpc_cache_capacity,
        rpc_cache_ttl_secs = args.rpc_cache_ttl_secs,
        channel_buffer_size = args.channel_buffer_size,
        "Starting audit archiver"
    );

    if args.kafka_properties_file.is_some() || args.kafka_topic.is_some() {
        warn!(
            "TIPS_AUDIT_KAFKA_PROPERTIES_FILE / TIPS_AUDIT_KAFKA_TOPIC are deprecated and ignored: \
             bundle events are now ingested over RPC via base_persistBatchedBundleEvent. \
             Remove these args from the deploy config."
        );
    }

    let s3_client = create_s3_client(&args).await?;
    let s3_bucket = args.s3_bucket.clone();
    let writer = S3EventReaderWriter::new(s3_client, s3_bucket);

    let dedup_cache: Cache<String, ()> = Cache::builder()
        .max_capacity(args.rpc_cache_capacity)
        .eviction_policy(EvictionPolicy::lru())
        .time_to_live(Duration::from_secs(args.rpc_cache_ttl_secs))
        .build();

    let (event_tx, event_rx) = mpsc::channel(args.channel_buffer_size);
    let reader = RpcEventReader::new(event_rx);

    let rpc_addr = SocketAddr::from(([0, 0, 0, 0], args.rpc_port));
    let rpc_module =
        AuditArchiverRpc::with_bundle_events(Arc::new(writer.clone()), dedup_cache, event_tx);
    let rpc_server = ServerBuilder::default()
        .build(rpc_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build RPC server: {e}"))?;
    let rpc_handle = rpc_server.start(rpc_module.into_rpc());
    info!(rpc_addr = %rpc_addr, "Audit archiver RPC server started");

    let mut archiver = AuditArchiver::new(
        reader,
        writer,
        args.worker_pool_size,
        args.channel_buffer_size,
        args.noop_archive,
    );

    info!("Audit archiver initialized, starting main loop");

    tokio::select! {
        result = archiver.run() => result,
        _ = rpc_handle.stopped() => {
            Err(anyhow::anyhow!("RPC server stopped unexpectedly"))
        }
    }
}

async fn create_s3_client(args: &Args) -> Result<S3Client> {
    match args.s3_config_type {
        S3ConfigType::Manual => {
            let region = args.s3_region.clone();
            let mut config_builder =
                aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region));

            if let Some(endpoint) = &args.s3_endpoint {
                config_builder = config_builder.endpoint_url(endpoint);
            }

            if let (Some(access_key), Some(secret_key)) =
                (&args.s3_access_key_id, &args.s3_secret_access_key)
            {
                let credentials = Credentials::new(access_key, secret_key, None, None, "manual");
                config_builder = config_builder.credentials_provider(credentials);
            }

            let config = config_builder.load().await;
            let s3_config_builder = S3ConfigBuilder::from(&config).force_path_style(true);

            info!(message = "manually configuring s3 client");
            Ok(S3Client::from_conf(s3_config_builder.build()))
        }
        S3ConfigType::Aws => {
            info!(message = "using aws s3 client");
            let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
            Ok(S3Client::new(&config))
        }
    }
}
