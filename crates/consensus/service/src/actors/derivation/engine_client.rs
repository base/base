use std::fmt::Debug;

use async_trait::async_trait;
use base_protocol::{AttributesWithParent, L2BlockInfo};
use derive_more::Constructor;
use tokio::sync::{mpsc, oneshot};

use crate::{
    EngineActorRequest, EngineClientError, EngineClientResult, ResetOrigin, ResetReason,
    ResetRequest, SafeL2SignalRequest,
};

/// Client to use to interact with the engine.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DerivationEngineClient: Debug + Send + Sync {
    /// Resets the engine's forkchoice.
    async fn reset_engine_forkchoice(&self, reason: ResetReason) -> EngineClientResult<()>;

    /// Sends a request to finalize the L2 block at the provided block number.
    /// Note: This does not wait for the engine to process it.
    async fn send_finalized_l2_block(&self, block_number: u64) -> EngineClientResult<()>;

    /// Sends derived attributes to the engine with a lock-step confirmation oneshot.
    ///
    /// Note: This does not wait for the engine to process it.
    async fn send_derived_attributes(
        &self,
        attributes: AttributesWithParent,
        confirmed: oneshot::Sender<L2BlockInfo>,
    ) -> EngineClientResult<()>;

    /// Sends a delegated safe L2 head to the engine. Confirmation is mailbox-only.
    ///
    /// Note: This does not wait for the engine to process it.
    async fn send_delegated_safe_head(&self, safe_l2: L2BlockInfo) -> EngineClientResult<()>;
}

/// Client to use to send messages to the Engine Actor's inbound channel.
#[derive(Clone, Constructor, Debug)]
pub struct QueuedDerivationEngineClient {
    /// A channel to use to send the [`EngineActorRequest`]s to the `EngineActor`.
    pub engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
}

#[async_trait]
impl DerivationEngineClient for QueuedDerivationEngineClient {
    async fn reset_engine_forkchoice(&self, reason: ResetReason) -> EngineClientResult<()> {
        let (result_tx, mut result_rx) = mpsc::channel(1);

        info!(target: "derivation", "Sending reset request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ResetRequest(Box::new(ResetRequest {
                result_tx,
                origin: ResetOrigin::Derivation,
                reason,
            })))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        result_rx
            .recv()
            .await
            .inspect(|_| info!(target: "derivation", "Engine reset successfully."))
            .ok_or_else(|| {
                error!(target: "derivation_engine_client", "Failed to receive forkchoice reset result");
                EngineClientError::ResponseError("response channel closed.".to_string())
            })?
    }

    async fn send_finalized_l2_block(&self, block_number: u64) -> EngineClientResult<()> {
        trace!(target: "derivation", block_number, "Sending finalized L2 block number to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(Box::new(block_number)))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        Ok(())
    }

    async fn send_derived_attributes(
        &self,
        attributes: AttributesWithParent,
        confirmed: oneshot::Sender<L2BlockInfo>,
    ) -> EngineClientResult<()> {
        trace!(target: "derivation", ?attributes, "Sending derived attributes to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessSafeL2SignalRequest(Box::new(
                SafeL2SignalRequest::derived(attributes, confirmed),
            )))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        Ok(())
    }

    async fn send_delegated_safe_head(&self, safe_l2: L2BlockInfo) -> EngineClientResult<()> {
        trace!(target: "derivation", ?safe_l2, "Sending delegated safe L2 head to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessSafeL2SignalRequest(Box::new(
                SafeL2SignalRequest::delegated(safe_l2),
            )))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        Ok(())
    }
}
