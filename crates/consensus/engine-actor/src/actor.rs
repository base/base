//! Direct engine actor implementation.
//!
//! This module provides [`DirectEngineActor`] which implements kona's [`NodeActor`] trait
//! using our transport-agnostic [`DirectEngineApi`] instead of kona's HTTP-bound `EngineClient`.

use std::sync::Arc;

use async_trait::async_trait;
use kona_node_service::{EngineActorRequest, EngineContext, EngineError, NodeActor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{DirectEngineApi, DirectEngineProcessor};

/// Direct engine actor for in-process execution layer communication.
///
/// This actor implements kona's [`NodeActor`] trait but uses our transport-agnostic
/// [`DirectEngineApi`] instead of kona's HTTP-bound `EngineClient`. This enables
/// direct in-process communication with reth's `BeaconConsensusEngine`.
///
/// # Type Parameters
///
/// - `E`: The engine API implementation, must implement [`DirectEngineApi`]
#[derive(Debug)]
pub struct DirectEngineActor<E>
where
    E: DirectEngineApi,
{
    /// Token for coordinated cancellation.
    cancellation_token: CancellationToken,

    /// The engine processor that handles request processing.
    processor: DirectEngineProcessor<E>,
}

impl<E> DirectEngineActor<E>
where
    E: DirectEngineApi,
{
    /// Creates a new `DirectEngineActor`.
    pub const fn new(
        cancellation_token: CancellationToken,
        processor: DirectEngineProcessor<E>,
    ) -> Self {
        Self { cancellation_token, processor }
    }

    /// Creates a new `DirectEngineActor` with all components.
    ///
    /// This is a convenience constructor that builds the processor internally.
    pub fn with_components(
        cancellation_token: CancellationToken,
        inbound_rx: mpsc::Receiver<EngineActorRequest>,
        client: Arc<E>,
        rollup: Arc<kona_genesis::RollupConfig>,
    ) -> Self {
        let processor =
            DirectEngineProcessor::new(client, rollup, inbound_rx, cancellation_token.clone());
        Self { cancellation_token, processor }
    }

    /// Returns a reference to the cancellation token.
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Returns a reference to the engine processor.
    pub const fn processor(&self) -> &DirectEngineProcessor<E> {
        &self.processor
    }
}

#[async_trait]
impl<E> NodeActor for DirectEngineActor<E>
where
    E: DirectEngineApi + std::fmt::Debug + 'static,
{
    type Error = EngineError;
    type StartData = EngineContext;

    async fn start(self, ctx: Self::StartData) -> Result<(), Self::Error> {
        info!(target: "engine-actor", "DirectEngineActor starting");

        // Delegate to the processor's start method
        self.processor.start(ctx).await.map_err(|e| {
            error!(target: "engine-actor", ?e, "DirectEngineProcessor failed");
            EngineError::ChannelClosed
        })
    }
}

/// Error type for engine actor operations.
#[derive(Debug, thiserror::Error)]
pub enum DirectEngineActorError {
    /// The inbound request channel was closed unexpectedly.
    #[error("inbound request channel closed")]
    ChannelClosed,

    /// Error from the engine API.
    #[error("engine API error: {0}")]
    EngineApi(String),

    /// Error from the processor.
    #[error("processor error: {0}")]
    Processor(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = DirectEngineActorError::ChannelClosed;
        assert_eq!(err.to_string(), "inbound request channel closed");

        let err = DirectEngineActorError::EngineApi("test".to_string());
        assert_eq!(err.to_string(), "engine API error: test");

        let err = DirectEngineActorError::Processor("test".to_string());
        assert_eq!(err.to_string(), "processor error: test");
    }
}
