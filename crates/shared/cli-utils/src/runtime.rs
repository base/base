//! Tokio runtime utilities for CLI applications.
//!
//! This module provides utilities for building and managing Tokio runtimes
//! with graceful shutdown handling via Ctrl+C signal interception.

use std::future::Future;

/// Builds a multi-threaded Tokio runtime with all features enabled.
///
/// This function creates a Tokio runtime configured for optimal performance
/// in production workloads, with thread scheduling enabled across all
/// available CPU cores.
///
/// # Returns
///
/// Returns an [`eyre::Result`] containing the configured [`tokio::runtime::Runtime`],
/// or an error if the runtime could not be built.
///
/// # Example
///
/// ```no_run
/// use base_cli_utils::runtime::build_runtime;
///
/// let runtime = build_runtime().expect("Failed to build runtime");
/// runtime.block_on(async {
///     println!("Hello from the runtime!");
/// });
/// ```
pub fn build_runtime() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre::eyre!("Failed to build tokio runtime: {}", e))
}

/// Runs a future to completion with graceful Ctrl+C shutdown handling.
///
/// This function executes the provided future while simultaneously listening
/// for a Ctrl+C signal. If Ctrl+C is received, the function returns immediately
/// with `Ok(())`, allowing the application to perform graceful shutdown.
///
/// # Arguments
///
/// * `fut` - The future to run to completion.
///
/// # Returns
///
/// Returns `Ok(())` on successful completion or Ctrl+C interruption.
/// Returns an error if the Ctrl+C handler fails to install.
///
/// # Example
///
/// ```no_run
/// use base_cli_utils::runtime::{build_runtime, run_until_ctrl_c};
///
/// async fn my_long_running_task() {
///     loop {
///         // Do some work
///         tokio::time::sleep(std::time::Duration::from_secs(1)).await;
///     }
/// }
///
/// let runtime = build_runtime().expect("Failed to build runtime");
/// runtime.block_on(async {
///     run_until_ctrl_c(my_long_running_task()).await.expect("Runtime error");
/// });
/// ```
pub async fn run_until_ctrl_c<F>(fut: F) -> eyre::Result<()>
where
    F: Future<Output = ()>,
{
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    tokio::select! {
        biased;

        () = ctrl_c => {
            Ok(())
        }
        () = fut => {
            Ok(())
        }
    }
}

/// Runs a fallible future to completion with graceful Ctrl+C shutdown handling.
///
/// This is similar to [`run_until_ctrl_c`], but accepts a future that returns
/// a [`Result`]. If the future completes with an error, that error is propagated.
/// If Ctrl+C is received, the function returns `Ok(())`.
///
/// # Arguments
///
/// * `fut` - The fallible future to run to completion.
///
/// # Returns
///
/// Returns `Ok(())` on successful completion or Ctrl+C interruption.
/// Returns the future's error if it fails, or an error if the Ctrl+C handler
/// fails to install.
///
/// # Example
///
/// ```no_run
/// use base_cli_utils::runtime::{build_runtime, run_until_ctrl_c_fallible};
///
/// async fn my_fallible_task() -> eyre::Result<()> {
///     // Do some work that might fail
///     Ok(())
/// }
///
/// let runtime = build_runtime().expect("Failed to build runtime");
/// runtime.block_on(async {
///     run_until_ctrl_c_fallible(my_fallible_task()).await.expect("Runtime error");
/// });
/// ```
pub async fn run_until_ctrl_c_fallible<F>(fut: F) -> eyre::Result<()>
where
    F: Future<Output = eyre::Result<()>>,
{
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    tokio::select! {
        biased;

        () = ctrl_c => {
            Ok(())
        }
        result = fut => {
            result
        }
    }
}
