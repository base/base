use std::fmt::Debug;

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use thiserror::Error;
use tokio::sync::mpsc;

/// Client used to schedule unsafe [`BaseExecutionPayloadEnvelope`] to be gossiped.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait UnsafePayloadGossipClient: Send + Sync + Debug {
    /// Returns whether payloads should be sealed privately without commit or gossip.
    fn seals_privately(&self) -> bool {
        false
    }

    /// This is a fire-and-forget function that schedules the provided
    /// [`BaseExecutionPayloadEnvelope`] to be gossiped. The implementation should return as
    /// quickly as possible and offers no guarantees that the payload actually was gossiped
    /// successfully.
    async fn schedule_execution_payload_gossip(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError>;
}

/// Errors that can occur when using the [`UnsafePayloadGossipClient`].
#[derive(Debug, Error)]
pub enum UnsafePayloadGossipClientError {
    /// Error sending request.
    #[error("Error sending request: {0}")]
    RequestError(String),
}

/// Queued implementation of [`UnsafePayloadGossipClient`] that handles requests by sending them
/// to a handler via the contained sender.
#[derive(Debug, Clone)]
pub struct QueuedUnsafePayloadGossipClient {
    /// Queue used to relay unsafe payloads to gossip.
    request_tx: Option<mpsc::Sender<BaseExecutionPayloadEnvelope>>,
}

impl QueuedUnsafePayloadGossipClient {
    /// Creates a payload gossip client backed by the network actor.
    pub const fn new(request_tx: mpsc::Sender<BaseExecutionPayloadEnvelope>) -> Self {
        Self { request_tx: Some(request_tx) }
    }

    /// Creates a private-sealing capability without a network actor.
    pub const fn private() -> Self {
        Self { request_tx: None }
    }
}

#[async_trait]
impl UnsafePayloadGossipClient for QueuedUnsafePayloadGossipClient {
    fn seals_privately(&self) -> bool {
        self.request_tx.is_none()
    }

    async fn schedule_execution_payload_gossip(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        let Some(request_tx) = &self.request_tx else {
            return Err(UnsafePayloadGossipClientError::RequestError(
                "payload gossip disabled".to_string(),
            ));
        };
        request_tx.send(payload).await.map_err(|send_error| {
            let err = UnsafePayloadGossipClientError::RequestError("request channel closed".to_string());
            error!(target: "gossip_client", payload = ?send_error.0, ?err, "failed to request to gossip payload.");
            err
        })
    }
}
