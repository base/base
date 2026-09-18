use std::fmt::Debug;

use async_trait::async_trait;
use base_consensus_derive::Signal;
use base_protocol::L2BlockInfo;
use tokio::sync::mpsc;

use crate::{DerivationActorRequest, DerivationClientError, DerivationClientResult};

/// Client to use to interact with the [`crate::DerivationActor`].
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait EngineDerivationClient: Debug + Send + Sync {
    /// Notifies the [`crate::DerivationActor`] that engine syncing has completed.
    /// Note: Does not wait for the derivation client to process this message.
    async fn notify_sync_completed(&self, safe_head: L2BlockInfo) -> DerivationClientResult<()>;

    /// Sends the new engine `safe_head` to the [`crate::DerivationActor`].
    /// Note: Does not wait for the derivation client to process this message.
    async fn send_new_engine_safe_head(&self, safe_head: L2BlockInfo)
    -> DerivationClientResult<()>;

    /// Sends the [`crate::DerivationActor`] the provided [`Signal`].
    /// Note: Does not wait for the derivation client to process this message.
    async fn send_signal(&self, signal: Signal) -> DerivationClientResult<()>;
}

/// Client to use to send messages to the [`crate::DerivationActor`]'s inbound channel.
#[derive(Debug)]
pub enum QueuedEngineDerivationClient {
    /// Requests are sent to a derivation actor.
    Enabled {
        /// Channel used to send [`DerivationActorRequest`]s to the derivation actor.
        derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>,
    },
    /// Requests are accepted without a derivation actor.
    Disabled,
}

impl QueuedEngineDerivationClient {
    /// Creates an enabled derivation client.
    pub const fn new(derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>) -> Self {
        Self::Enabled { derivation_actor_request_tx }
    }

    /// Creates a disabled derivation client for a node without a derivation actor.
    pub const fn disabled() -> Self {
        Self::Disabled
    }
}

#[async_trait]
impl EngineDerivationClient for QueuedEngineDerivationClient {
    async fn notify_sync_completed(&self, safe_head: L2BlockInfo) -> DerivationClientResult<()> {
        match self {
            Self::Enabled { derivation_actor_request_tx } => {
                info!(target: "engine", "Sending sync completed to derivation actor");
                derivation_actor_request_tx
                    .send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(
                        safe_head,
                    )))
                    .await
                    .map_err(|_| {
                        DerivationClientError::RequestError("request channel closed.".to_string())
                    })
            }
            Self::Disabled => {
                info!(target: "engine", "Ignoring sync completed notification: derivation client disabled");
                Ok(())
            }
        }
    }

    async fn send_new_engine_safe_head(
        &self,
        safe_head: L2BlockInfo,
    ) -> DerivationClientResult<()> {
        match self {
            Self::Enabled { derivation_actor_request_tx } => {
                info!(target: "engine", safe_head = ?safe_head, "Sending new safe head to derivation actor");
                derivation_actor_request_tx
                    .send(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(
                        safe_head,
                    )))
                    .await
                    .map_err(|_| {
                        DerivationClientError::RequestError("request channel closed.".to_string())
                    })
            }
            Self::Disabled => {
                info!(target: "engine", safe_head = ?safe_head, "Ignoring new engine safe head: derivation client disabled");
                Ok(())
            }
        }
    }

    async fn send_signal(&self, signal: Signal) -> DerivationClientResult<()> {
        match self {
            Self::Enabled { derivation_actor_request_tx } => {
                info!(target: "engine", signal = ?signal, "Sending signal to derivation actor");
                derivation_actor_request_tx
                    .send(DerivationActorRequest::ProcessEngineSignalRequest(Box::new(signal)))
                    .await
                    .map_err(|_| {
                        DerivationClientError::RequestError("request channel closed.".to_string())
                    })
            }
            Self::Disabled => {
                info!(target: "engine", signal = ?signal, "Ignoring signal: derivation client disabled");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use base_protocol::L2BlockInfo;

    use super::{EngineDerivationClient, QueuedEngineDerivationClient};

    #[tokio::test]
    async fn disabled_client_accepts_sync_completion_without_an_actor() {
        let client = QueuedEngineDerivationClient::Disabled;

        let result = client.notify_sync_completed(L2BlockInfo::default()).await;

        assert!(result.is_ok());
    }
}
