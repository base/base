//! Consensus client wrapper.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{FutureExt, future::BoxFuture};
use tracing::info;

use crate::engine::EngineEndpoint;

/// Handle to a running consensus client.
#[must_use = "Dropping the handle will stop the consensus client"]
pub struct ConsensusHandle {
    fut: BoxFuture<'static, eyre::Result<()>>,
}

impl fmt::Debug for ConsensusHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConsensusHandle").finish_non_exhaustive()
    }
}

impl ConsensusHandle {
    /// Creates a new [`ConsensusHandle`] from a future.
    pub fn new(fut: impl Future<Output = eyre::Result<()>> + Send + 'static) -> Self {
        Self { fut: fut.boxed() }
    }
}

impl Future for ConsensusHandle {
    type Output = eyre::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.as_mut().poll(cx)
    }
}

/// Wrapper around the consensus client.
#[derive(Debug, Clone)]
pub struct Consensus {
    engine_endpoint: EngineEndpoint,
    chain_id: u64,
}

impl Consensus {
    /// Creates a new [`Consensus`] instance.
    pub const fn new(engine_endpoint: EngineEndpoint, chain_id: u64) -> Self {
        Self { engine_endpoint, chain_id }
    }

    /// Runs the consensus client, returning a [`ConsensusHandle`].
    pub fn run(self) -> ConsensusHandle {
        ConsensusHandle::new(async move {
            info!(
                target: "unified",
                endpoint = %self.engine_endpoint.url,
                chain_id = %self.chain_id,
                "[STUB] Consensus node is running"
            );

            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        })
    }
}
