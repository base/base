//! Tokio runtime utilities with graceful Ctrl+C shutdown handling.

use std::future::Future;

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

    /// Run a fallible future until ctrl-c is pressed.
    pub fn run_until_ctrl_c<F>(fut: F) -> eyre::Result<()>
    where
        F: Future<Output = eyre::Result<()>>,
    {
        let rt = Self::tokio_runtime().map_err(|e| eyre::eyre!(e))?;
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
