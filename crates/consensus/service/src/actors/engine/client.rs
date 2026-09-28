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
#[derive(Debug, Clone)]
pub struct QueuedEngineDerivationClient {
    /// Channel to the derivation actor.
    derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>,
}

impl QueuedEngineDerivationClient {
    /// Creates a derivation client backed by the derivation actor's inbound channel.
    pub const fn new(derivation_actor_request_tx: mpsc::Sender<DerivationActorRequest>) -> Self {
        Self { derivation_actor_request_tx }
    }
}

#[async_trait]
impl EngineDerivationClient for QueuedEngineDerivationClient {
    async fn notify_sync_completed(&self, safe_head: L2BlockInfo) -> DerivationClientResult<()> {
        info!(target: "engine", "Sending sync completed to derivation actor");
        self.derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(safe_head)))
            .await
            .map_err(|_| DerivationClientError::RequestError("request channel closed.".to_string()))
    }

    async fn send_new_engine_safe_head(
        &self,
        safe_head: L2BlockInfo,
    ) -> DerivationClientResult<()> {
        info!(target: "engine", safe_head = ?safe_head, "Sending new safe head to derivation actor");
        self.derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(safe_head)))
            .await
            .map_err(|_| DerivationClientError::RequestError("request channel closed.".to_string()))
    }

    async fn send_signal(&self, signal: Signal) -> DerivationClientResult<()> {
        info!(target: "engine", signal = ?signal, "Sending signal to derivation actor");
        self.derivation_actor_request_tx
            .send(DerivationActorRequest::ProcessEngineSignalRequest(Box::new(signal)))
            .await
            .map_err(|_| DerivationClientError::RequestError("request channel closed.".to_string()))
    }
}

/// Derivation client for a node that runs without a [`crate::DerivationActor`].
///
/// Every request is accepted and dropped, so the engine needs no disabled-mode branching.
#[derive(Debug, Clone)]
pub struct DisabledEngineDerivationClient;

#[async_trait]
impl EngineDerivationClient for DisabledEngineDerivationClient {
    async fn notify_sync_completed(&self, _safe_head: L2BlockInfo) -> DerivationClientResult<()> {
        debug!(target: "engine", "derivation disabled; ignoring sync completion");
        Ok(())
    }

    async fn send_new_engine_safe_head(
        &self,
        _safe_head: L2BlockInfo,
    ) -> DerivationClientResult<()> {
        debug!(target: "engine", "derivation disabled; ignoring new engine safe head");
        Ok(())
    }

    async fn send_signal(&self, _signal: Signal) -> DerivationClientResult<()> {
        debug!(target: "engine", "derivation disabled; ignoring signal");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_consensus_derive::Signal;
    use base_protocol::L2BlockInfo;

    use super::{DisabledEngineDerivationClient, EngineDerivationClient};

    #[tokio::test]
    async fn disabled_client_accepts_sync_completion_without_an_actor() {
        let client = DisabledEngineDerivationClient;

        let result = client.notify_sync_completed(L2BlockInfo::default()).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn disabled_client_accepts_safe_head_updates_without_an_actor() {
        let client = DisabledEngineDerivationClient;

        let result = client.send_new_engine_safe_head(L2BlockInfo::default()).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn disabled_client_accepts_signals_without_an_actor() {
        let client = DisabledEngineDerivationClient;

        let result = client.send_signal(Signal::FlushChannel).await;

        assert!(result.is_ok());
    }
}
