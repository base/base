//! Full challenger service lifecycle.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use eyre::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::ChallengerConfig;

/// Default health server bind address.
const HEALTH_ADDR: &str = "0.0.0.0";

/// Default health server port.
const HEALTH_PORT: u16 = 8080;

/// Runs the full challenger service lifecycle.
///
/// Steps:
/// 1. Initialise logging, TLS, and metrics
/// 2. Install signal handlers for graceful shutdown
/// 3. Start health HTTP server
/// 4. Wait for SIGTERM or SIGINT
/// 5. Graceful shutdown in reverse order
pub async fn run(config: ChallengerConfig) -> Result<()> {
    config.log.init_tracing_subscriber()?;

    // Install the default rustls CryptoProvider before any TLS connections are created.
    let _ = rustls::crypto::ring::default_provider().install_default();

    info!(version = env!("CARGO_PKG_VERSION"), "Challenger starting");

    // ── 1. Global cancellation token and signal handler ──────────────────
    let cancel = CancellationToken::new();
    crate::SignalHandler::install(cancel.clone());

    // ── 2. Metrics recorder (if enabled) ─────────────────────────────────
    config.metrics.init().expect("failed to install Prometheus recorder");

    // Record startup metrics (no-ops if no recorder installed).
    crate::record_startup_metrics(env!("CARGO_PKG_VERSION"));

    // ── 3. Start health HTTP server ──────────────────────────────────────
    // Ready flag is hardcoded to false — no driver is wired yet.
    let ready = Arc::new(AtomicBool::new(false));
    let health_handle: JoinHandle<Result<()>> = {
        let addr: SocketAddr = format!("{HEALTH_ADDR}:{HEALTH_PORT}").parse()?;
        let ready_flag = Arc::clone(&ready);
        let health_cancel = cancel.clone();
        tokio::spawn(async move { crate::serve(addr, ready_flag, health_cancel).await })
    };

    info!("Service initialised, waiting for shutdown signal");

    // ── 4. Wait for shutdown signal ──────────────────────────────────────
    cancel.cancelled().await;
    info!("Shutdown signal received, stopping service...");

    // ── 5. Graceful shutdown (reverse initialisation order) ──────────────
    ready.store(false, Ordering::SeqCst);

    match health_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
        Err(e) => warn!(error = %e, "Health server task panicked"),
    }

    info!("Service stopped");
    Ok(())
}
