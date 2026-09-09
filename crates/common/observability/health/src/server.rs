//! Standalone health-check HTTP server.
//!
//! Provides:
//! - `GET /healthz` — liveness probe (always 200 while the process is alive)
//! - `GET /readyz`  — readiness probe (200 when the service is fully initialised)
//!
//! This is a lightweight axum-based server intended for services that need
//! standard Kubernetes-style health probes without a full JSON-RPC stack.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{Router, extract::State, http::StatusCode, routing::get};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
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
///
/// Exposes `GET /healthz` (liveness) and `GET /readyz` (readiness) endpoints
/// on a dedicated HTTP port.
#[derive(Debug)]
pub struct HealthServer;

impl HealthServer {
    /// Returns an [`axum::Router`] with `/healthz` and `/readyz` routes.
    ///
    /// Use this when you need a custom listener setup or want to compose
    /// health routes with other middleware. For the common case of a
    /// standalone health server, prefer [`serve`](Self::serve) instead.
    pub fn router(ready: Arc<AtomicBool>) -> Router {
        let state = ServerState { ready };
        Router::new()
            .route("/healthz", get(ServerState::liveness))
            .route("/readyz", get(ServerState::readiness))
            .with_state(state)
    }

    /// Starts the health HTTP server.
    ///
    /// The server binds to `addr` and runs until the cancellation token is
    /// fired.
    ///
    /// # Arguments
    ///
    /// * `addr`   — socket address to listen on (e.g. `0.0.0.0:8080`)
    /// * `ready`  — shared flag; `/readyz` returns 200 when this is `true`
    /// * `cancel` — token that triggers graceful shutdown
    ///
    /// # Errors
    ///
    /// Returns an error if the TCP listener cannot bind to `addr`.
    pub async fn serve(
        addr: SocketAddr,
        ready: Arc<AtomicBool>,
        cancel: CancellationToken,
    ) -> eyre::Result<()> {
        let app = Self::router(ready);

        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "Health server started");

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await?;

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

    use rstest::rstest;
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Starts the health server on an ephemeral port and returns its address
    /// along with a cancellation token for graceful shutdown.
    async fn start_test_server(
        ready: Arc<AtomicBool>,
    ) -> (SocketAddr, JoinHandle<()>, CancellationToken) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = HealthServer::router(ready);
        let cancel = CancellationToken::new();
        let cancel_for_shutdown = cancel.clone();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel_for_shutdown.cancelled().await })
                .await
                .unwrap();
        });

        (addr, handle, cancel)
    }

    #[rstest]
    #[case::liveness_always_ok(false, "/healthz", 200)]
    #[case::readiness_not_ready(false, "/readyz", 503)]
    #[case::readiness_ready(true, "/readyz", 200)]
    #[tokio::test]
    async fn test_health_endpoint(
        #[case] initial_ready: bool,
        #[case] endpoint: &str,
        #[case] expected_status: u16,
    ) {
        let ready = Arc::new(AtomicBool::new(initial_ready));
        let (addr, _handle, cancel) = start_test_server(ready).await;

        let resp = reqwest::get(format!("http://{addr}{endpoint}")).await.unwrap();
        assert_eq!(resp.status(), expected_status);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_readiness_transitions() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, _handle, cancel) = start_test_server(Arc::clone(&ready)).await;

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

        cancel.cancel();
    }
}
