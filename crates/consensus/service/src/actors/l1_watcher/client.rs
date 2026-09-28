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
#[derive(Debug, Clone)]
pub struct QueuedL1WatcherDerivationClient {
    /// Channel used to send [`DerivationActorRequest`]s to the derivation actor.
    derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>,
}

impl QueuedL1WatcherDerivationClient {
    /// Creates a derivation client backed by the derivation actor's inbound channel.
    pub const fn new(derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>) -> Self {
        Self { derivation_actor_request_tx }
    }
}

#[async_trait]
impl L1WatcherDerivationClient for QueuedL1WatcherDerivationClient {
    async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()> {
        trace!(target: "l1_watcher", ?block, "Sending finalized l1 block to derivation actor.");
        self.derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessFinalizedL1Block(Box::new(block)))
            .await
            .map_err(|_| DerivationClientError::RequestError("request channel closed.".to_string()))
    }

    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
        trace!(target: "l1_watcher", ?block, "Sending new l1 head to derivation actor.");
        self.derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessL1HeadUpdateRequest(Box::new(block)))
            .await
            .map_err(|_| DerivationClientError::RequestError("request channel closed.".to_string()))
    }
}

/// Derivation client for a node that runs without a [`crate::DerivationActor`].
///
/// Every request is accepted and dropped, so the L1 watcher needs no disabled-mode branching.
#[derive(Debug, Clone)]
pub struct DisabledL1WatcherDerivationClient;

#[async_trait]
impl L1WatcherDerivationClient for DisabledL1WatcherDerivationClient {
    async fn send_finalized_l1_block(&self, _block: BlockInfo) -> DerivationClientResult<()> {
        trace!(target: "l1_watcher", "derivation disabled; ignoring finalized l1 block");
        Ok(())
    }

    async fn send_new_l1_head(&self, _block: BlockInfo) -> DerivationClientResult<()> {
        trace!(target: "l1_watcher", "derivation disabled; ignoring new l1 head");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_protocol::BlockInfo;

    use super::{DisabledL1WatcherDerivationClient, L1WatcherDerivationClient};

    #[tokio::test]
    async fn disabled_client_accepts_finalized_block_without_an_actor() {
        let client = DisabledL1WatcherDerivationClient;

        let result = client.send_finalized_l1_block(BlockInfo::default()).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn disabled_client_accepts_new_head_without_an_actor() {
        let client = DisabledL1WatcherDerivationClient;

        let result = client.send_new_l1_head(BlockInfo::default()).await;

        assert!(result.is_ok());
    }
}
