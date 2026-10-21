//! HTTP server scaffold for Roxy.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
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
    /// Starts the Roxy HTTP server with the provided configuration.
    pub async fn serve(config: Config, cancel: CancellationToken) -> anyhow::Result<()> {
        let listen_addr = config.listen_addr;
        let ready = Arc::new(AtomicBool::new(false));
        let app = HealthServer::router(Arc::clone(&ready));

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
