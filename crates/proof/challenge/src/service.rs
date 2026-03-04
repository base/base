//! Full challenger service lifecycle.

use std::sync::{Arc, atomic::AtomicBool};

use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::ChallengerConfig;

/// Top-level challenger service.
#[derive(Debug)]
pub struct ChallengerService;

impl ChallengerService {
    /// Runs the full challenger service lifecycle.
    ///
    /// This is the scaffolding entry point for the challenger. It sets up
    /// infrastructure (logging, TLS, metrics, health endpoints) and blocks
    /// until the runtime is shut down (e.g. via `RuntimeManager::run_until_ctrl_c`).
    /// No business logic (dispute-game monitoring, proof generation, or
    /// challenge submission) is wired yet — those will be added in subsequent
    /// steps.
    ///
    /// # Lifecycle
    ///
    /// 1. Initialise logging, TLS, and metrics
    /// 2. Start health HTTP server (`/readyz` returns 503 — no driver wired yet)
    /// 3. Block until runtime shutdown
    ///
    /// # Errors
    ///
    /// Returns an error if tracing initialisation fails, the Prometheus
    /// recorder cannot be installed, or the health HTTP server cannot bind.
    pub async fn run(config: ChallengerConfig) -> Result<()> {
        config.log.init_tracing_subscriber()?;

        // Install the default rustls CryptoProvider before any TLS connections are created.
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Challenger starting");

        // ── 1. Metrics recorder (if enabled) ─────────────────────────────────
        config
            .metrics
            .init()
            .map_err(|e| eyre::eyre!("failed to install Prometheus recorder: {e}"))?;

        // Record startup metrics (no-ops if no recorder installed).
        crate::ChallengerMetrics::record_startup(env!("CARGO_PKG_VERSION"));

        // ── 2. Start health HTTP server ──────────────────────────────────────
        // Ready flag is hardcoded to false — no driver is wired yet.
        // The CancellationToken is never explicitly cancelled; when
        // `run_until_ctrl_c` receives Ctrl-C the runtime drops and aborts
        // all spawned tasks.
        let ready = Arc::new(AtomicBool::new(false));
        let health_cancel = CancellationToken::new();
        let health_handle = {
            let addr = config.health_addr;
            let ready_flag = Arc::clone(&ready);
            let cancel = health_cancel;
            tokio::spawn(async move { crate::HealthServer::serve(addr, ready_flag, cancel).await })
        };

        info!("Service initialised, waiting for shutdown signal");

        // ── 3. Await health server (runs until runtime shutdown) ─────────────
        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        info!("Service stopped");
        Ok(())
    }
}
