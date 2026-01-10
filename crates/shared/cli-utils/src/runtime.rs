//! Tokio runtime utilities with graceful Ctrl+C shutdown handling.

use std::future::Future;

/// Builds a multi-threaded Tokio runtime with all features enabled.
pub fn build_runtime() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre::eyre!("Failed to build tokio runtime: {}", e))
}

/// Runs a future to completion, returning early on Ctrl+C.
pub async fn run_until_ctrl_c<F>(fut: F) -> eyre::Result<()>
where
    F: Future<Output = ()>,
{
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
    };

    tokio::select! {
        biased;
        () = ctrl_c => Ok(()),
        () = fut => Ok(()),
    }
}

/// Runs a fallible future to completion, returning early on Ctrl+C.
pub async fn run_until_ctrl_c_fallible<F>(fut: F) -> eyre::Result<()>
where
    F: Future<Output = eyre::Result<()>>,
{
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
    };

    tokio::select! {
        biased;
        () = ctrl_c => Ok(()),
        result = fut => result,
    }
}
