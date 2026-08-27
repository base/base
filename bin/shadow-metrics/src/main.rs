//! Shadow-metrics service entry point.

use std::net::SocketAddr;

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
    DEFAULT_DATABASE, DEFAULT_PORT, DEFAULT_USERNAME, PgConnectionParams, ShadowMetricsStore,
    api_router,
};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info};

base_cli_utils::define_log_args!("SHADOW_METRICS");
base_cli_utils::define_metrics_args!("SHADOW_METRICS", 9003);

#[derive(Debug, Clone)]
struct HealthState {
    store: Option<ShadowMetricsStore>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    log: LogArgs,

    #[command(flatten)]
    metrics: MetricsArgs,

    /// Postgres host; unset disables reader and database readiness checks.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_HOST")]
    postgres_host: Option<String>,

    /// Password for the Postgres role.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_PASSWORD")]
    postgres_password: Option<String>,

    /// Postgres port.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_PORT", default_value_t = DEFAULT_PORT)]
    postgres_port: u16,

    /// Postgres database name.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_DATABASE", default_value = DEFAULT_DATABASE)]
    postgres_database: String,

    /// Postgres role to authenticate as.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_USER", default_value = DEFAULT_USERNAME)]
    postgres_user: String,

    /// Maximum Postgres pool connections.
    #[arg(long, env = "SHADOW_METRICS_POSTGRES_MAX_CONNECTIONS", default_value = "10")]
    postgres_max_connections: u32,

    /// Health server port.
    #[arg(long, env = "SHADOW_METRICS_HTTP_PORT", default_value = "9101")]
    http_port: u16,
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

    let connection = match (&args.postgres_host, &args.postgres_password) {
        (Some(host), Some(password)) => Some(PgConnectionParams {
            host: host.clone(),
            port: args.postgres_port,
            database: args.postgres_database.clone(),
            username: args.postgres_user.clone(),
            password: password.clone(),
        }),
        // Connecting with an empty password would surface as an opaque Postgres auth
        // failure rather than a configuration error.
        (Some(_), None) => anyhow::bail!(
            "--postgres-host (env SHADOW_METRICS_POSTGRES_HOST) requires --postgres-password (env \
             SHADOW_METRICS_POSTGRES_PASSWORD)"
        ),
        (None, _) => None,
    };

    let store = match &connection {
        Some(connection) => {
            Some(ShadowMetricsStore::connect(connection, args.postgres_max_connections).await?)
        }
        None => None,
    };

    info!(
        http_addr = %http_addr,
        metrics_addr = %args.metrics.addr,
        metrics_port = args.metrics.port,
        postgres_enabled = connection.is_some(),
        "Starting shadow-metrics service"
    );

    let app = health_router(store.clone()).merge(api_router(store));
    let http_listener = TcpListener::bind(http_addr).await?;
    let http_server = axum::serve(http_listener, app);
    info!(http_addr = %http_addr, "Shadow-metrics HTTP server started (health + block API)");

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

/// Waits for SIGINT or SIGTERM.
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

fn health_router(store: Option<ShadowMetricsStore>) -> Router {
    Router::new()
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(HealthState { store })
}

async fn healthz_handler() -> &'static str {
    "ok\n"
}

async fn readyz_handler(State(state): State<HealthState>) -> Response {
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
        let state = HealthState { store: None };

        let response = readyz_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::OK);
    }
}
