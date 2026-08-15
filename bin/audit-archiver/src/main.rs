//! Audit archiver binary entry point.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use audit_archiver_lib::{
    AuditArchiver, AuditArchiverApiServer, AuditArchiverRpc, DEFAULT_TRANSACTION_EVENT_BATCH_PATH,
    DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE, DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES,
    DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES, DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES,
    PgTransactionEventSink, RpcEventReader, S3EventReaderWriter, TransactionEventIngestConfig,
};
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, config::Builder as S3ConfigBuilder};
use axum::{
    BoxError,
    error_handling::HandleErrorLayer,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base_cli_utils::LogConfig;
use clap::{Parser, ValueEnum};
use jsonrpsee::server::{ServerBuilder, stop_channel};
use moka::{policy::EvictionPolicy, sync::Cache};
use tokio::{net::TcpListener, sync::mpsc};
use tower::ServiceBuilder;
use tracing::{error, info};

base_cli_utils::define_log_args!("TIPS_AUDIT");
base_cli_utils::define_metrics_args!("TIPS_AUDIT", 9002);

#[derive(Debug, Clone, ValueEnum)]
enum S3ConfigType {
    Aws,
    Manual,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Command {
    Serve,
    Migrate,
}

/// Postgres migration action for the `migrate` command.
///
/// Only `up` is supported because audit-archiver migrations are intended to be
/// forward-only operational changes.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum MigrationDirection {
    Up,
}

#[derive(Debug, Clone)]
struct HealthState {
    transaction_event_sink: Option<PgTransactionEventSink>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(value_enum, default_value_t = Command::Serve)]
    command: Command,

    #[arg(value_enum)]
    migration_direction: Option<MigrationDirection>,

    #[arg(long, env = "TIPS_AUDIT_S3_BUCKET")]
    s3_bucket: Option<String>,

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

    /// Postgres connection URL for transaction observability events. When unset,
    /// the HTTP transaction-event ingest endpoint is disabled.
    #[arg(long, env = "TIPS_AUDIT_POSTGRES_URL")]
    postgres_url: Option<String>,

    /// Maximum Postgres connections used by the transaction-event ingest sink.
    #[arg(long, env = "TIPS_AUDIT_POSTGRES_MAX_CONNECTIONS", default_value = "10")]
    postgres_max_connections: u32,

    /// HTTP path for Vector transaction-event batch ingest.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_HTTP_PATH",
        default_value = DEFAULT_TRANSACTION_EVENT_BATCH_PATH
    )]
    transaction_event_http_path: String,

    /// Maximum transaction events accepted in one HTTP batch.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_MAX_BATCH_SIZE",
        default_value_t = DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE
    )]
    transaction_event_max_batch_size: usize,

    /// Maximum serialized JSON bytes accepted for one transaction event.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_MAX_EVENT_BYTES",
        default_value_t = DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES
    )]
    transaction_event_max_event_bytes: usize,

    /// Maximum serialized JSON bytes accepted for one transaction event's data field.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_MAX_DATA_BYTES",
        default_value_t = DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES
    )]
    transaction_event_max_data_bytes: usize,

    /// Maximum HTTP request body size for transaction event ingest.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_MAX_REQUEST_BYTES",
        default_value_t = DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES
    )]
    transaction_event_max_request_bytes: usize,
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

    if matches!(args.command, Command::Migrate) {
        run_migrations(&args).await?;
        return Ok(());
    }

    run_server(args).await
}

async fn run_migrations(args: &Args) -> Result<()> {
    if !matches!(args.migration_direction, Some(MigrationDirection::Up)) {
        anyhow::bail!("migration command requires an explicit direction: migrate up");
    }

    let postgres_url = args
        .postgres_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("TIPS_AUDIT_POSTGRES_URL must be set for migrations"))?;

    info!("Running audit archiver Postgres migrations");
    PgTransactionEventSink::migrate(postgres_url).await?;
    info!("Audit archiver Postgres migrations complete");
    Ok(())
}

async fn run_server(args: Args) -> Result<()> {
    let s3_bucket = args
        .s3_bucket
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TIPS_AUDIT_S3_BUCKET must be set for serve"))?;

    info!(
        s3_bucket = %s3_bucket,
        metrics_addr = %args.metrics.addr,
        metrics_port = args.metrics.port,
        rpc_port = args.rpc_port,
        transaction_event_http_path = %args.transaction_event_http_path,
        transaction_event_http_enabled = args.postgres_url.is_some(),
        rpc_cache_capacity = args.rpc_cache_capacity,
        rpc_cache_ttl_secs = args.rpc_cache_ttl_secs,
        channel_buffer_size = args.channel_buffer_size,
        "Starting audit archiver"
    );

    let s3_client = create_s3_client(&args).await?;
    let writer = S3EventReaderWriter::new(s3_client, s3_bucket);

    let dedup_cache: Cache<String, ()> = Cache::builder()
        .max_capacity(args.rpc_cache_capacity)
        .eviction_policy(EvictionPolicy::lru())
        .time_to_live(Duration::from_secs(args.rpc_cache_ttl_secs))
        .build();

    let (event_tx, event_rx) = mpsc::channel(args.channel_buffer_size);
    let reader = RpcEventReader::new(event_rx);

    let rpc_addr = SocketAddr::from(([0, 0, 0, 0], args.rpc_port));
    let transaction_event_sink = if let Some(postgres_url) = &args.postgres_url {
        Some(PgTransactionEventSink::connect(postgres_url, args.postgres_max_connections).await?)
    } else {
        None
    };

    let mut rpc_module =
        AuditArchiverRpc::with_bundle_events(Arc::new(writer.clone()), dedup_cache, event_tx);
    if let Some(sink) = transaction_event_sink.clone() {
        rpc_module = rpc_module.with_transaction_event_store(sink);
    }
    // The jsonrpsee service is driven by the axum listener below. Keep the
    // stop handle passed into the service builder; axum owns the HTTP server
    // lifecycle for this combined RPC and transaction-event endpoint.
    let (rpc_stop_handle, _rpc_server_handle) = stop_channel();
    let rpc_service =
        ServerBuilder::default().to_service_builder().build(rpc_module.into_rpc(), rpc_stop_handle);
    let rpc_service = ServiceBuilder::new()
        .layer(HandleErrorLayer::new(|error: BoxError| async move {
            error!(error = %error, "audit archiver RPC service error");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
        }))
        .service(rpc_service);

    let health_router = health_router(transaction_event_sink.clone());
    let http_app = if let Some(sink) = transaction_event_sink {
        let config = TransactionEventIngestConfig {
            path: args.transaction_event_http_path.clone(),
            max_batch_size: args.transaction_event_max_batch_size,
            max_event_bytes: args.transaction_event_max_event_bytes,
            max_data_bytes: args.transaction_event_max_data_bytes,
            max_request_bytes: args.transaction_event_max_request_bytes,
        };
        let path = config.path.clone();
        info!(rpc_addr = %rpc_addr, %path, "transaction event HTTP ingest enabled on audit RPC server");
        config.into_router(Arc::new(sink)).merge(health_router).fallback_service(rpc_service)
    } else {
        info!("transaction event HTTP ingest disabled; TIPS_AUDIT_POSTGRES_URL is not set");
        health_router.fallback_service(rpc_service)
    };

    let http_listener = TcpListener::bind(rpc_addr).await?;
    let http_server = axum::serve(http_listener, http_app);
    info!(rpc_addr = %rpc_addr, "Audit archiver HTTP server started");

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
        result = http_server => {
            result.map_err(|e| anyhow::anyhow!("audit archiver HTTP server stopped unexpectedly: {e}"))
        }
    }
}

fn health_router(transaction_event_sink: Option<PgTransactionEventSink>) -> axum::Router {
    axum::Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(HealthState { transaction_event_sink })
}

async fn healthz_handler() -> &'static str {
    "ok\n"
}

async fn readyz_handler(State(state): State<HealthState>) -> Response {
    match PgTransactionEventSink::check_optional_schema_ready(state.transaction_event_sink.as_ref())
        .await
    {
        Ok(()) => (StatusCode::OK, "ready\n".to_string()).into_response(),
        Err(err) => {
            error!(error = %err, "audit archiver readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready\n".to_string()).into_response()
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
