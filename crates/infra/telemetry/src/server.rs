use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context;
use axum::Router;
use base_health::HealthServer;
use clap::Args;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Configuration for the Base telemetry HTTP server.
#[derive(Args, Debug, Clone)]
pub struct ServerConfig {
    /// Socket address to bind the HTTP server to.
    #[arg(long, env = "BASE_TELEMETRY_LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen_addr: SocketAddr,
}

/// Base telemetry Axum server scaffold.
#[derive(Debug, Clone, Copy, Default)]
pub struct BaseTelemetryServer;

impl BaseTelemetryServer {
    /// Returns the application router for the telemetry service.
    pub fn router(ready: Arc<AtomicBool>) -> Router {
        HealthServer::router(ready)
    }

    /// Starts the telemetry service with the provided configuration.
    pub async fn serve(config: ServerConfig, cancel: CancellationToken) -> anyhow::Result<()> {
        let ready = Arc::new(AtomicBool::new(false));
        let app = Self::router(Arc::clone(&ready));
        let listener = TcpListener::bind(config.listen_addr).await.with_context(|| {
            format!("failed to bind base telemetry server to {}", config.listen_addr)
        })?;
        let listen_addr =
            listener.local_addr().context("failed to read base telemetry listen address")?;

        ready.store(true, Ordering::SeqCst);

        info!(listen_addr = %listen_addr, "base telemetry server started");

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .context("base telemetry server exited unexpectedly")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, atomic::AtomicBool},
    };

    use axum::http::StatusCode;
    use tokio::{net::TcpListener, task::JoinHandle};

    use crate::BaseTelemetryServer;

    async fn start_test_server() -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ready = Arc::new(AtomicBool::new(true));
        let handle = tokio::spawn(async move {
            axum::serve(listener, BaseTelemetryServer::router(ready)).await.unwrap();
        });

        (addr, handle)
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let (addr, handle) = start_test_server().await;

        let response = reqwest::get(format!("http://{addr}/healthz")).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        handle.abort();
    }

    #[tokio::test]
    async fn readyz_returns_ok() {
        let (addr, handle) = start_test_server().await;

        let response = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        handle.abort();
    }
}
