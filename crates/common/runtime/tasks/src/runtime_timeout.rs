//! Runtime-backed timeout helpers.

use std::{future::IntoFuture, time::Duration};

use futures::future::{Either, select};

use crate::Clock;

/// Runs futures against timeout deadlines supplied by a [`Clock`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeTimeout;

impl RuntimeTimeout {
    /// Runs `future` until completion or until `duration` elapses on `runtime`.
    pub async fn run<R, F>(runtime: &R, duration: Duration, future: F) -> Result<F::Output, ()>
    where
        R: Clock,
        F: IntoFuture,
        F::IntoFuture: Send,
    {
        match select(Box::pin(future.into_future()), runtime.sleep(duration)).await {
            Either::Left((output, _)) => Ok(output),
            Either::Right(((), _)) => Err(()),
        }
    }
}
