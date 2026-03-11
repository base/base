//! Tokio runtime utilities with graceful shutdown handling.
//!
//! Provides [`RuntimeManager`] for creating Tokio runtimes and installing
//! OS signal handlers (SIGINT + SIGTERM on unix, SIGINT on other platforms)
//! that cancel a [`CancellationToken`] for cooperative shutdown.

use std::future::Future;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// A runtime manager.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeManager;

impl RuntimeManager {
    /// Creates a new default tokio multi-thread [Runtime](tokio::runtime::Runtime) with all
    /// features enabled.
    pub fn tokio_runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()
    }

    /// Installs SIGTERM + SIGINT handlers that cancel the given token.
    ///
    /// On unix, this listens for both SIGINT and SIGTERM. On other platforms,
    /// only SIGINT (Ctrl-C) is handled. When a signal is received the
    /// [`CancellationToken`] is cancelled, allowing all holders of child tokens
    /// to begin cooperative shutdown.
    pub fn install_signal_handler(cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            let signal = Self::wait_for_shutdown_signal().await;
            info!(signal, "received shutdown signal");

            cancel.cancel();
        })
    }

    /// Run a fallible future until a shutdown signal is received.
    ///
    /// On unix, this listens for both SIGINT and SIGTERM. On other platforms,
    /// only SIGINT (Ctrl-C) is handled.
    pub fn run_until_ctrl_c<F>(fut: F) -> eyre::Result<()>
    where
        F: Future<Output = eyre::Result<()>>,
    {
        let rt = Self::tokio_runtime().map_err(|e| eyre::eyre!(e))?;
        rt.block_on(async move {
            tokio::select! {
                biased;
                signal = Self::wait_for_shutdown_signal() => {
                    info!(target: "cli", signal, "Received shutdown signal, shutting down...");
                    Ok(())
                }
                res = fut => res,
            }
        })
    }

    async fn wait_for_shutdown_signal() -> &'static str {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result.expect("failed to listen for SIGINT");
                    "SIGINT"
                }
                _ = sigterm.recv() => "SIGTERM",
            }
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.expect("failed to listen for SIGINT");
            "SIGINT"
        }
    }
}
