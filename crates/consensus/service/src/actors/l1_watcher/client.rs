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
pub struct QueuedL1WatcherDerivationClient {
    /// A channel to use to send the [`DerivationActorRequest`]s to the [`crate::DerivationActor`].
    pub derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>,
    /// Whether requests should be sent to a derivation actor.
    pub enabled: bool,
}

impl QueuedL1WatcherDerivationClient {
    /// Creates an enabled derivation client.
    pub const fn new(derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>) -> Self {
        Self { derivation_actor_request_tx, enabled: true }
    }

    /// Creates a disabled derivation client for a node without a derivation actor.
    pub const fn disabled(
        derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>,
    ) -> Self {
        Self { derivation_actor_request_tx, enabled: false }
    }
}

#[async_trait]
impl L1WatcherDerivationClient for QueuedL1WatcherDerivationClient {
    async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()> {
        if !self.enabled {
            return Ok(());
        }
        trace!(target: "l1_watcher", ?block, "Sending finalized l1 block to derivation actor.");
        let _ = self
            .derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessFinalizedL1Block(Box::new(block)))
            .await
            .map_err(|_| {
                DerivationClientError::RequestError("request channel closed.".to_string())
            })?;

        Ok(())
    }

    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
        if !self.enabled {
            return Ok(());
        }
        trace!(target: "l1_watcher", ?block, "Sending new l1 head to derivation actor.");
        self.derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessL1HeadUpdateRequest(Box::new(block)))
            .await
            .map_err(|_| {
                DerivationClientError::RequestError("request channel closed.".to_string())
            })?;

        Ok(())
    }
}
