//! Health-check HTTP server.
//!
//! Provides:
//! - `GET /healthz` — liveness probe (always 200 while the process is alive)
//! - `GET /readyz`  — readiness probe (200 when the service is fully initialised)

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{Router, extract::State, http::StatusCode, routing::get};
use tokio::net::TcpListener;
use tracing::info;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// State shared across all HTTP handlers.
#[derive(Clone)]
struct ServerState {
    /// Set to `true` once the service has completed initialisation.
    ready: Arc<AtomicBool>,
}

impl ServerState {
    /// `GET /healthz` — liveness probe.
    async fn liveness() -> StatusCode {
        StatusCode::OK
    }

    /// `GET /readyz` — readiness probe.
    async fn readiness(State(state): State<Self>) -> StatusCode {
        if state.ready.load(Ordering::Relaxed) {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Health-check HTTP server.
#[derive(Debug)]
pub struct HealthServer;

impl HealthServer {
    /// Starts the health HTTP server.
    ///
    /// The server binds to `addr` and runs until the task is aborted.
    ///
    /// # Arguments
    ///
    /// * `addr`  — socket address to listen on (e.g. `0.0.0.0:8080`)
    /// * `ready` — shared flag; `/readyz` returns 200 when this is `true`
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind to `addr`.
    pub async fn serve(addr: SocketAddr, ready: Arc<AtomicBool>) -> eyre::Result<()> {
        let state = ServerState { ready };

        let app = Router::new()
            .route("/healthz", get(ServerState::liveness))
            .route("/readyz", get(ServerState::readiness))
            .with_state(state);

        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "Health server started");

        axum::serve(listener, app).await?;

        info!("Health server stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use tokio::task::JoinHandle;

    use super::*;

    /// Starts the health server on an ephemeral port and returns its address.
    async fn start_test_server(ready: Arc<AtomicBool>) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let state = ServerState { ready };

        let app = Router::new()
            .route("/healthz", get(ServerState::liveness))
            .route("/readyz", get(ServerState::readiness))
            .with_state(state);

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (addr, handle)
    }

    #[tokio::test]
    async fn test_liveness_always_ok() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, handle) = start_test_server(ready).await;

        let resp = reqwest::get(format!("http://{addr}/healthz")).await.unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }

    #[tokio::test]
    async fn test_readiness_not_ready() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, handle) = start_test_server(ready).await;

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 503);

        handle.abort();
    }

    #[tokio::test]
    async fn test_readiness_ready() {
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, handle) = start_test_server(ready).await;

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }

    #[tokio::test]
    async fn test_readiness_transitions() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, handle) = start_test_server(Arc::clone(&ready)).await;

        // Initially not ready
        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 503);

        // Mark as ready
        ready.store(true, Ordering::SeqCst);

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Mark as not ready (shutdown)
        ready.store(false, Ordering::SeqCst);

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 503);

        handle.abort();
    }
}
