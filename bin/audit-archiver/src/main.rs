//! Audit archiver binary entry point.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use audit_archiver_lib::{
    AuditArchiverApiServer, AuditArchiverRpc, DEFAULT_TRANSACTION_EVENT_BATCH_PATH,
    DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS, DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS,
    DEFAULT_TRANSACTION_EVENT_MAX_BATCH_SIZE, DEFAULT_TRANSACTION_EVENT_MAX_DATA_BYTES,
    DEFAULT_TRANSACTION_EVENT_MAX_EVENT_BYTES, DEFAULT_TRANSACTION_EVENT_MAX_REQUEST_BYTES,
    DEFAULT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS,
    DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS,
    DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS, Metrics, PgTransactionEventSink,
    TransactionEventIngestConfig, TransactionEventRetentionConfig,
};
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
use tokio::{
    net::TcpListener,
    time::{MissedTickBehavior, interval},
};
use tower::ServiceBuilder;
use tracing::{error, info};

/// Delay before retrying partition maintenance after a failed or blocked pass.
///
/// A pod that starts before its network's migration fails its first pass. It
/// should pick up the new schema, and report a truthful partition horizon,
/// within a minute rather than a full retention interval.
const PARTITION_MAINTENANCE_RETRY: Duration = Duration::from_secs(60);

base_cli_utils::define_log_args!("TIPS_AUDIT");
base_cli_utils::define_metrics_args!("TIPS_AUDIT", 9002);

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
    transaction_event_sink: PgTransactionEventSink,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(value_enum, default_value_t = Command::Serve)]
    command: Command,

    #[arg(value_enum)]
    migration_direction: Option<MigrationDirection>,

    #[command(flatten)]
    log: LogArgs,

    #[command(flatten)]
    metrics: MetricsArgs,

    #[arg(long, env = "TIPS_AUDIT_RPC_PORT", default_value = "9100")]
    rpc_port: u16,

    /// Postgres connection URL for transaction observability events. Required
    /// when serving HTTP ingest and RPC queries.
    #[arg(long, env = "TIPS_AUDIT_POSTGRES_URL")]
    postgres_url: Option<String>,

    /// Maximum Postgres connections used by the transaction-event ingest sink.
    #[arg(long, env = "TIPS_AUDIT_POSTGRES_MAX_CONNECTIONS", default_value = "10")]
    postgres_max_connections: u32,

    /// Seconds between transaction-event partition maintenance passes. The
    /// first pass runs immediately at startup; later passes wait this
    /// interval. Each pass creates upcoming day partitions and drops expired
    /// ones.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS",
        default_value_t = DEFAULT_TRANSACTION_EVENT_RETENTION_INTERVAL_SECS
    )]
    transaction_event_retention_interval_secs: u64,

    /// Days to keep high-volume proxy and builder-decision events, by
    /// `event_time`.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_HOT_RETENTION_DAYS",
        default_value_t = DEFAULT_TRANSACTION_EVENT_HOT_RETENTION_DAYS
    )]
    transaction_event_hot_retention_days: u32,

    /// Days to keep ingress, simulation-success, and txpool-forward events, by
    /// `event_time`.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_WARM_RETENTION_DAYS",
        default_value_t = DEFAULT_TRANSACTION_EVENT_WARM_RETENTION_DAYS
    )]
    transaction_event_warm_retention_days: u32,

    /// Days to keep failures, drops, inclusion, and flashblock events, by
    /// `event_time`.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_COLD_RETENTION_DAYS",
        default_value_t = DEFAULT_TRANSACTION_EVENT_COLD_RETENTION_DAYS
    )]
    transaction_event_cold_retention_days: u32,

    /// Postgres `lock_timeout` for one partition create, detach, or drop, in
    /// milliseconds.
    ///
    /// Detach briefly queues inserts for its retention class, so a blocked
    /// statement gives up after this timeout and retries on the next pass.
    #[arg(
        long,
        env = "TIPS_AUDIT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS",
        default_value_t = DEFAULT_TRANSACTION_EVENT_PARTITION_LOCK_TIMEOUT_MS
    )]
    transaction_event_partition_lock_timeout_ms: u64,

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
    let postgres_url = args
        .postgres_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("TIPS_AUDIT_POSTGRES_URL must be set for serve"))?;

    let retention_config = TransactionEventRetentionConfig {
        hot_days: args.transaction_event_hot_retention_days,
        warm_days: args.transaction_event_warm_retention_days,
        cold_days: args.transaction_event_cold_retention_days,
        partition_lock_timeout_ms: args.transaction_event_partition_lock_timeout_ms,
        interval_secs: args.transaction_event_retention_interval_secs,
    }
    .validate()?;
    let retention_interval = Duration::from_secs(retention_config.interval_secs);

    info!(
        metrics_addr = %args.metrics.addr,
        metrics_port = args.metrics.port,
        rpc_port = args.rpc_port,
        transaction_event_http_path = %args.transaction_event_http_path,
        transaction_event_hot_retention_days = retention_config.hot_days,
        transaction_event_warm_retention_days = retention_config.warm_days,
        transaction_event_cold_retention_days = retention_config.cold_days,
        transaction_event_partition_lock_timeout_ms = retention_config.partition_lock_timeout_ms,
        transaction_event_retention_interval_secs = retention_interval.as_secs(),
        "Starting audit archiver"
    );

    let rpc_addr = SocketAddr::from(([0, 0, 0, 0], args.rpc_port));
    let transaction_event_sink =
        PgTransactionEventSink::connect(postgres_url, args.postgres_max_connections)
            .await?
            .with_retention_config(retention_config)?;
    transaction_event_sink.check_schema_ready().await?;

    let rpc_module = AuditArchiverRpc::new(transaction_event_sink.clone());
    // The jsonrpsee service is driven by the axum listener below. Keep the
    // stop handle passed into the service builder; axum owns the HTTP server
    // lifecycle for this combined RPC and transaction-event endpoint.
    let (rpc_stop_handle, _rpc_server_handle) = stop_channel();
    // SECURITY: These unauthenticated ingest APIs are internal endpoints.
    // Deployments must restrict them to trusted producers on a private network.
    let rpc_service =
        ServerBuilder::default().to_service_builder().build(rpc_module.into_rpc(), rpc_stop_handle);
    let rpc_service = ServiceBuilder::new()
        .layer(HandleErrorLayer::new(|error: BoxError| async move {
            error!(error = %error, "audit archiver RPC service error");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
        }))
        .service(rpc_service);

    let retention_sink = transaction_event_sink.clone();
    let health_router = health_router(transaction_event_sink.clone());
    let config = TransactionEventIngestConfig {
        path: args.transaction_event_http_path.clone(),
        max_batch_size: args.transaction_event_max_batch_size,
        max_event_bytes: args.transaction_event_max_event_bytes,
        max_data_bytes: args.transaction_event_max_data_bytes,
        max_request_bytes: args.transaction_event_max_request_bytes,
    };
    let path = config.path.clone();
    info!(rpc_addr = %rpc_addr, %path, "transaction event HTTP ingest enabled on audit RPC server");
    let http_app = config
        .into_router(Arc::new(transaction_event_sink))
        .merge(health_router)
        .fallback_service(rpc_service);

    let http_listener = TcpListener::bind(rpc_addr).await?;
    // Kubernetes sends SIGTERM (after the preStop sleep) when it replaces a pod.
    // The binary runs as PID 1, which ignores SIGTERM without a handler, so
    // without this the old pod keeps answering kept-alive connections until
    // SIGKILL. Graceful shutdown stops accepting, finishes in-flight requests,
    // and closes idle keep-alive connections so clients reconnect to new pods.
    let http_server = axum::serve(http_listener, http_app).with_graceful_shutdown(async {
        let signal = shutdown_signal().await;
        info!(signal, "audit archiver shutting down HTTP server");
    });
    info!(rpc_addr = %rpc_addr, "Audit archiver HTTP server started");

    let retention_worker = run_retention_worker(retention_sink, retention_interval);

    tokio::select! {
        result = http_server => {
            result.map_err(|e| anyhow::anyhow!("audit archiver HTTP server stopped unexpectedly: {e}"))?;
            info!("audit archiver HTTP server stopped");
            Ok(())
        }
        result = retention_worker => result,
    }
}

async fn run_retention_worker(
    transaction_event_sink: PgTransactionEventSink,
    retention_interval: Duration,
) -> Result<()> {
    // First tick is immediate so a new replica creates today's and upcoming
    // partitions without waiting a full interval. Skip missed ticks so a slow
    // pass does not catch up. A failed pass, or one whose DDL hit a lock
    // timeout, retries after PARTITION_MAINTENANCE_RETRY instead.
    let mut ticker = interval(retention_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let retry_after = PARTITION_MAINTENANCE_RETRY.min(retention_interval);

    loop {
        ticker.tick().await;
        let retry = match transaction_event_sink.maintain_partitions().await {
            Ok(outcome)
                if outcome.partitions_created > 0
                    || outcome.partitions_dropped > 0
                    || outcome.lock_timeouts > 0 =>
            {
                info!(
                    partitions_created = outcome.partitions_created,
                    partitions_dropped = outcome.partitions_dropped,
                    lock_timeouts = outcome.lock_timeouts,
                    "transaction event partition maintenance changed partitions"
                );
                outcome.lock_timeouts > 0
            }
            Ok(_) => false,
            Err(err) => {
                Metrics::transaction_event_retention_failures().increment(1);
                error!(
                    error = %err,
                    retry_secs = retry_after.as_secs(),
                    "transaction event partition maintenance failed"
                );
                true
            }
        };
        if retry {
            ticker.reset_after(retry_after);
        }
    }
}

/// Waits for SIGTERM (Kubernetes pod shutdown) or SIGINT.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
    }
}

/// Waits for Ctrl-C on non-Unix targets.
#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "ctrl_c"
}

fn health_router(transaction_event_sink: PgTransactionEventSink) -> axum::Router {
    axum::Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(HealthState { transaction_event_sink })
}

async fn healthz_handler() -> &'static str {
    "ok\n"
}

async fn readyz_handler(State(state): State<HealthState>) -> Response {
    match state.transaction_event_sink.check_schema_ready().await {
        Ok(()) => (StatusCode::OK, "ready\n".to_string()).into_response(),
        Err(err) => {
            error!(error = %err, "audit archiver readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready\n".to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serve_without_postgres_fails_before_accepting_events() {
        let mut args = Args::parse_from(["audit-archiver"]);
        args.postgres_url = None;

        let error = run_server(args).await.expect_err("serve must require durable event storage");
        assert!(error.to_string().contains("TIPS_AUDIT_POSTGRES_URL must be set for serve"));
    }
}
