//! Shadow-metrics service entry point.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base_cli_utils::LogConfig;
use base_shadow_metrics::{
    DEFAULT_MAX_ROWS_PER_POLL, DEFAULT_POLL_INTERVAL_SECS, ShadowMetricsReader,
    ShadowMetricsReaderConfig, ShadowMetricsStore,
};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info};

base_cli_utils::define_log_args!("SHADOW_METRICS");
base_cli_utils::define_metrics_args!("SHADOW_METRICS", 9003);

#[derive(Debug, Clone)]
struct HealthState {
    store: Option<ShadowMetricsStore>,
    /// `None` when no reader is configured. Otherwise the flag is cleared once the
    /// reader task stops, so `/readyz` fails and Kubernetes restarts the pod rather
    /// than leaving a healthy-looking service that emits nothing.
    reader_alive: Option<Arc<AtomicBool>>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    log: LogArgs,

    #[command(flatten)]
    metrics: MetricsArgs,

    /// Postgres connection URL. When unset, Postgres connectivity is disabled,
    /// no reader is started, and `/readyz` always reports ready.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_URL")]
    postgres_url: Option<String>,

    /// Maximum Postgres pool connections.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_MAX_CONNECTIONS", default_value = "10")]
    postgres_max_connections: u32,

    /// Health server port.
    #[arg(long, env = "SHADOW_METRICS_HTTP_PORT", default_value = "9101")]
    http_port: u16,

    /// Interval in seconds between shadow block polls. The writer flushes every
    /// second and reconciliation lands in bursts roughly every ten seconds, so a
    /// short interval keeps emission close to real time without idle-spinning on
    /// Postgres.
    #[arg(
        long,
        env = "SHADOW_METRICS_POLL_INTERVAL_SECS",
        default_value_t = DEFAULT_POLL_INTERVAL_SECS
    )]
    poll_interval_secs: u64,

    /// Maximum shadow block rows fetched by one poll. Bounds catch-up so a long
    /// outage drains steadily instead of loading every JSONB payload at once.
    #[arg(
        long,
        env = "SHADOW_METRICS_MAX_ROWS_PER_POLL",
        default_value_t = DEFAULT_MAX_ROWS_PER_POLL
    )]
    max_rows_per_poll: u32,
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

    run_server(args).await
}

async fn run_server(args: Args) -> Result<()> {
    let http_addr = SocketAddr::from(([0, 0, 0, 0], args.http_port));

    let store = if let Some(postgres_url) = &args.postgres_url {
        Some(ShadowMetricsStore::connect(postgres_url, args.postgres_max_connections).await?)
    } else {
        None
    };

    info!(
        http_addr = %http_addr,
        metrics_addr = %args.metrics.addr,
        metrics_port = args.metrics.port,
        postgres_enabled = args.postgres_url.is_some(),
        poll_interval_secs = args.poll_interval_secs,
        max_rows_per_poll = args.max_rows_per_poll,
        "Starting shadow-metrics service"
    );

    // Starting the reader before the health server is deliberate. `ShadowMetricsReader::new`
    // resolves and persists the starting cursor, so it fails closed on an unreachable or
    // unmigrated Postgres and aborts startup for Kubernetes to retry. Serving `/readyz` while
    // silently running without a reader would be worse.
    let reader_alive = match &store {
        Some(store) => {
            let config = ShadowMetricsReaderConfig {
                poll_interval: Duration::from_secs(args.poll_interval_secs),
                max_rows_per_poll: args.max_rows_per_poll,
            };
            let reader = ShadowMetricsReader::new(store.clone(), config).await?;
            info!("Shadow-metrics reader started");
            Some(spawn_reader(reader))
        }
        None => {
            info!("Postgres is not configured; shadow-metrics reader is disabled");
            None
        }
    };

    let app = health_router(store, reader_alive);
    let http_listener = TcpListener::bind(http_addr).await?;
    let http_server = axum::serve(http_listener, app);
    info!(http_addr = %http_addr, "Shadow-metrics health server started");

    tokio::select! {
        result = http_server => {
            result.map_err(|e| anyhow::anyhow!("shadow-metrics health server stopped unexpectedly: {e}"))
        }
        () = shutdown_signal() => {
            info!("Shutdown signal received; stopping shadow-metrics");
            Ok(())
        }
    }
}

/// Spawns the reader poll loop and returns a liveness flag cleared when it stops.
///
/// `ShadowMetricsReader::run` loops forever and absorbs poll failures internally, so
/// the flag only clears on something genuinely unexpected. Detecting that is the point:
/// a dead reader must surface through `/readyz` instead of being assumed impossible.
fn spawn_reader(reader: ShadowMetricsReader) -> Arc<AtomicBool> {
    let alive = Arc::new(AtomicBool::new(true));
    let reader_alive = Arc::clone(&alive);
    let handle = tokio::spawn(reader.run());

    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => error!("shadow-metrics reader poll loop returned unexpectedly"),
            Ok(Err(error)) => error!(error = %error, "shadow-metrics reader poll loop failed"),
            Err(error) => error!(
                error = %error,
                panicked = error.is_panic(),
                "shadow-metrics reader task did not complete"
            ),
        }
        reader_alive.store(false, Ordering::Release);
    });

    alive
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

fn health_router(
    store: Option<ShadowMetricsStore>,
    reader_alive: Option<Arc<AtomicBool>>,
) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(HealthState { store, reader_alive })
}

async fn healthz_handler() -> &'static str {
    "ok\n"
}

async fn readyz_handler(State(state): State<HealthState>) -> Response {
    // The reader's death is already logged with the real cause by `spawn_reader`
    // when its task stops; don't re-log here, or every probe floods the log.
    if state.reader_alive.as_ref().is_some_and(|alive| !alive.load(Ordering::Acquire)) {
        return (StatusCode::SERVICE_UNAVAILABLE, "not ready\n").into_response();
    }

    let readiness = match &state.store {
        Some(store) => store.check_schema_ready().await,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readyz_is_ready_without_postgres() {
        let state = HealthState { store: None, reader_alive: None };

        let response = readyz_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_unavailable_when_the_reader_died() {
        let state =
            HealthState { store: None, reader_alive: Some(Arc::new(AtomicBool::new(false))) };

        let response = readyz_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
