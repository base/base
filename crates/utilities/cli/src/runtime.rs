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
pub struct RuntimeManager {
    /// Worker thread stack size override.
    thread_stack_size: Option<usize>,
}

impl Default for RuntimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeManager {
    /// Creates a new [`RuntimeManager`] with default settings.
    pub const fn new() -> Self {
        Self { thread_stack_size: None }
    }

    /// Sets the worker thread stack size (in bytes).
    pub const fn with_thread_stack_size(mut self, size: usize) -> Self {
        self.thread_stack_size = Some(size);
        self
    }

    /// Creates a new default tokio multi-thread [Runtime](tokio::runtime::Runtime) with all
    /// features enabled.
    pub fn tokio_runtime(&self) -> Result<tokio::runtime::Runtime, std::io::Error> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if let Some(size) = self.thread_stack_size {
            builder.thread_stack_size(size);
        }
        builder.enable_all().build()
    }

    /// Installs SIGTERM + SIGINT handlers that cancel the given token.
    ///
    /// On unix, this listens for both SIGINT and SIGTERM. On other platforms,
    /// only SIGINT (Ctrl-C) is handled. When a signal is received the
    /// [`CancellationToken`] is cancelled, allowing all holders of child tokens
    /// to begin cooperative shutdown.
    pub fn install_signal_handler(cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        result.expect("failed to listen for SIGINT");
                        info!(signal = "SIGINT", "received shutdown signal");
                    }
                    _ = sigterm.recv() => {
                        info!(signal = "SIGTERM", "received shutdown signal");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.expect("failed to listen for SIGINT");
                info!(signal = "SIGINT", "received shutdown signal");
            }

            cancel.cancel();
        })
    }

    /// Run a fallible future until ctrl-c is pressed.
    pub fn run_until_ctrl_c<F>(&self, fut: F) -> eyre::Result<()>
    where
        F: Future<Output = eyre::Result<()>>,
    {
        let rt = self.tokio_runtime().map_err(|e| eyre::eyre!(e))?;
        rt.block_on(async move {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    info!(target: "cli", "Received Ctrl-C, shutting down...");
                    Ok(())
                }
                res = fut => res,
            }
        })
    }
}
