//! Runtime-backed timeout helpers.

use std::{future::IntoFuture, time::Duration};

use base_runtime::Runtime;

/// Runs futures against timeout deadlines supplied by a [`Runtime`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeTimeout;

impl RuntimeTimeout {
    /// Runs `future` until completion or until `duration` elapses on `runtime`.
    pub async fn run<R, F>(runtime: &R, duration: Duration, future: F) -> Result<F::Output, ()>
    where
        R: Runtime,
        F: IntoFuture,
        F::IntoFuture: Send,
    {
        let future = future.into_future();
        tokio::select! {
            biased;
            output = future => Ok(output),
            _ = runtime.sleep(duration) => Err(()),
        }
    }
}
