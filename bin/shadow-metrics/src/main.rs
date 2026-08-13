//! Shadow-metrics noop mock service entry point.

use std::{net::SocketAddr, time::Duration};

use anyhow::Result;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base_cli_utils::LogConfig;
use base_shadow_metrics::ShadowMetricsSink;
use clap::{Parser, ValueEnum};
use tokio::net::TcpListener;
use tracing::{error, info};

base_cli_utils::define_log_args!("SHADOW_METRICS");
base_cli_utils::define_metrics_args!("SHADOW_METRICS", 9003);

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Command {
    Serve,
    Migrate,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MigrationDirection {
    Up,
}

#[derive(Debug, Clone)]
struct HealthState {
    sink: Option<ShadowMetricsSink>,
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

    /// Postgres connection URL. When unset, Postgres connectivity is disabled
    /// and `/readyz` always reports ready.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_URL")]
    postgres_url: Option<String>,

    /// Maximum Postgres pool connections.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_MAX_CONNECTIONS", default_value = "10")]
    postgres_max_connections: u32,

    /// Health server port.
    #[arg(long, env = "SHADOW_METRICS_HTTP_PORT", default_value = "9101")]
    http_port: u16,

    /// Idle heartbeat log interval in seconds.
    #[arg(long, env = "SHADOW_METRICS_HEARTBEAT_INTERVAL_SECS", default_value = "30")]
    heartbeat_interval_secs: u64,
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
        .ok_or_else(|| anyhow::anyhow!("SHADOW_METRICS_POSTGRES_URL must be set for migrations"))?;

    info!("Running shadow-metrics Postgres migrations");
    ShadowMetricsSink::migrate(postgres_url).await?;
    info!("Shadow-metrics Postgres migrations complete");
    Ok(())
}

async fn run_server(args: Args) -> Result<()> {
    let http_addr = SocketAddr::from(([0, 0, 0, 0], args.http_port));

    let sink = if let Some(postgres_url) = &args.postgres_url {
        Some(ShadowMetricsSink::connect(postgres_url, args.postgres_max_connections).await?)
    } else {
        None
    };

    info!(
        http_addr = %http_addr,
        metrics_addr = %args.metrics.addr,
        metrics_port = args.metrics.port,
        postgres_enabled = args.postgres_url.is_some(),
        heartbeat_interval_secs = args.heartbeat_interval_secs,
        "Starting shadow-metrics noop service"
    );

    let app = health_router(sink);
    let http_listener = TcpListener::bind(http_addr).await?;
    let http_server = axum::serve(http_listener, app);
    info!(http_addr = %http_addr, "Shadow-metrics health server started");

    let heartbeat = run_heartbeat(Duration::from_secs(args.heartbeat_interval_secs));

    tokio::select! {
        result = heartbeat => result,
        result = http_server => {
            result.map_err(|e| anyhow::anyhow!("shadow-metrics health server stopped unexpectedly: {e}"))
        }
        () = shutdown_signal() => {
            info!("Shutdown signal received; stopping shadow-metrics");
            Ok(())
        }
    }
}

/// Resolves on SIGINT or, on Unix, SIGTERM so the k8s pre-kill SIGTERM unwinds
/// the runtime and drops the `PgPool` cleanly instead of a hard kill.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn run_heartbeat(interval: Duration) -> Result<()> {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        info!("shadow-metrics noop heartbeat");
    }
}

fn health_router(sink: Option<ShadowMetricsSink>) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(HealthState { sink })
}

async fn healthz_handler() -> &'static str {
    "ok\n"
}

async fn readyz_handler(State(state): State<HealthState>) -> Response {
    let readiness = match &state.sink {
        Some(sink) => sink.check_schema_ready().await,
        None => Ok(()),
    };
    match readiness {
        Ok(()) => (StatusCode::OK, "ready\n").into_response(),
        Err(err) => {
            error!(error = %err, "shadow-metrics readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response()
        }
    }
}
