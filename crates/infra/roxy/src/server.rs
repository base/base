//! HTTP server scaffold for Roxy.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use axum::Router;
use base_health::HealthServer;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::Config;

/// Roxy HTTP server.
///
/// Scaffold serves liveness and readiness probes. Proxy routes land in later
/// PRs on the same listener.
#[derive(Debug, Clone, Copy, Default)]
pub struct Server;

impl Server {
    /// Returns the application router.
    pub fn router(ready: Arc<AtomicBool>) -> Router {
        HealthServer::router(ready)
    }

    /// Starts the Roxy HTTP server with the provided configuration.
    pub async fn serve(config: Config, cancel: CancellationToken) -> anyhow::Result<()> {
        let listen_addr = config.listen_addr;
        let ready = Arc::new(AtomicBool::new(false));
        let app = Self::router(Arc::clone(&ready));

        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind roxy server to {listen_addr}"))?;
        let listen_addr = listener.local_addr().context("failed to read roxy listen address")?;

        info!(%listen_addr, "roxy server started");

        // Marked ready before `serve` is first polled. The listener is already bound, so the
        // kernel accept backlog queues any connections that arrive in the gap; they are served
        // as soon as the accept loop runs. Callers see added latency, never a refused connection.
        ready.store(true, Ordering::SeqCst);

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .context("roxy server exited with error")?;

        info!("roxy server stopped");
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

    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Starts the server on an ephemeral port for tests.
    async fn start_test_server(
        ready: Arc<AtomicBool>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>, CancellationToken) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let app = Server::router(ready);
        let cancel = CancellationToken::new();
        let cancel_for_shutdown = cancel.clone();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel_for_shutdown.cancelled().await })
                .await
                .expect("server serve");
        });

        (addr, handle, cancel)
    }

    #[tokio::test]
    async fn healthz_returns_ok_when_not_ready() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, _handle, cancel) = start_test_server(ready).await;

        let response =
            reqwest::get(format!("http://{addr}/healthz")).await.expect("healthz request");
        assert_eq!(response.status().as_u16(), 200, "liveness must return 200 even when not ready");

        cancel.cancel();
    }

    #[tokio::test]
    async fn readyz_reflects_ready_flag() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, _handle, cancel) = start_test_server(Arc::clone(&ready)).await;

        let response = reqwest::get(format!("http://{addr}/readyz"))
            .await
            .expect("readyz request while not ready");
        assert_eq!(response.status().as_u16(), 503, "readiness must return 503 before ready");

        ready.store(true, Ordering::SeqCst);

        let response = reqwest::get(format!("http://{addr}/readyz"))
            .await
            .expect("readyz request while ready");
        assert_eq!(response.status().as_u16(), 200, "readiness must return 200 after ready");

        cancel.cancel();
    }
}
