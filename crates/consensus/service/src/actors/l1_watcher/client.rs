use std::fmt::Debug;

use async_trait::async_trait;
use base_protocol::BlockInfo;
use tokio::sync::mpsc;

use crate::{DerivationActorRequest, DerivationClientError, DerivationClientResult};

/// Client to use to interact with the [`crate::DerivationActor`].
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait L1WatcherDerivationClient: Debug + Send + Sync {
    /// Sends the [`crate::DerivationActor`] the provided finalized L1 block.
    /// Note: this function just guarantees that it is received by the actor but does not have
    /// any insight into whether it was processed or processed successfully.
    async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()>;

    /// Sends the latest L1 Head to the [`crate::DerivationActor`].
    /// Note: this function just guarantees that it is received by the actor but does not have
    /// any insight into whether it was processed or processed successfully.
    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()>;
}

/// Client to use to send messages to the [`crate::DerivationActor`]'s inbound channel.
#[derive(Debug)]
pub enum QueuedL1WatcherDerivationClient {
    /// Requests are sent to a derivation actor.
    Enabled {
        /// Channel used to send [`DerivationActorRequest`]s to the derivation actor.
        derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>,
    },
    /// Requests are accepted without a derivation actor.
    Disabled,
}

impl QueuedL1WatcherDerivationClient {
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
impl L1WatcherDerivationClient for QueuedL1WatcherDerivationClient {
    async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()> {
        match self {
            Self::Enabled { derivation_actor_request_tx } => {
                trace!(target: "l1_watcher", ?block, "Sending finalized l1 block to derivation actor.");
                derivation_actor_request_tx
                    .send(DerivationActorRequest::ProcessFinalizedL1Block(Box::new(block)))
                    .await
                    .map_err(|_| {
                        DerivationClientError::RequestError("request channel closed.".to_string())
                    })
            }
            Self::Disabled => Ok(()),
        }
    }

    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
        match self {
            Self::Enabled { derivation_actor_request_tx } => {
                trace!(target: "l1_watcher", ?block, "Sending new l1 head to derivation actor.");
                derivation_actor_request_tx
                    .send(DerivationActorRequest::ProcessL1HeadUpdateRequest(Box::new(block)))
                    .await
                    .map_err(|_| {
                        DerivationClientError::RequestError("request channel closed.".to_string())
                    })
            }
            Self::Disabled => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_protocol::BlockInfo;

    use super::{L1WatcherDerivationClient, QueuedL1WatcherDerivationClient};

    #[tokio::test]
    async fn disabled_client_accepts_finalized_block_without_an_actor() {
        let client = QueuedL1WatcherDerivationClient::Disabled;

        let result = client.send_finalized_l1_block(BlockInfo::default()).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn disabled_client_accepts_new_head_without_an_actor() {
        let client = QueuedL1WatcherDerivationClient::Disabled;

        let result = client.send_new_l1_head(BlockInfo::default()).await;

        assert!(result.is_ok());
    }
}
