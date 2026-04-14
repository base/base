use std::fmt::Debug;

use async_trait::async_trait;
use base_consensus_engine::{ConsolidateInput, DelegatedForkchoiceUpdate};
use derive_more::Constructor;
use tokio::sync::mpsc;

use crate::{EngineActorRequest, EngineClientError, EngineClientResult, ResetRequest};

/// Client to use to interact with the engine.
#[cfg_attr(test, mockall::automock(type SafeL2Signal = OpAttributesWithParent;))]
#[async_trait]
pub trait DerivationEngineClient: Debug + Send + Sync {
    /// Resets the engine's forkchoice.
    async fn reset_engine_forkchoice(&self) -> EngineClientResult<()>;

    /// Sends follow-node delegated safe/finalized labels to the engine.
    ///
    /// Note: This does not wait for the engine to process the request.
    async fn send_delegated_forkchoice_update(
        &self,
        update: DelegatedForkchoiceUpdate,
    ) -> EngineClientResult<()>;

    /// Sends a request to finalize the L2 block at the provided block number.
    /// Note: This does not wait for the engine to process it.
    async fn send_finalized_l2_block(&self, block_number: u64) -> EngineClientResult<()>;

    /// Sends a consolidation signal to the engine.
    ///
    /// This is the unified entry point for all consolidation-related inputs,
    /// including derived attributes and safe L2 block information, as represented
    /// by [`ConsolidateInput`].
    ///
    /// Note: This does not wait for the engine to process it.
    async fn send_safe_l2_signal(&self, signal: ConsolidateInput) -> EngineClientResult<()>;
}

/// Client to use to send messages to the Engine Actor's inbound channel.
#[derive(Clone, Constructor, Debug)]
pub struct QueuedDerivationEngineClient {
    /// A channel to use to send the [`EngineActorRequest`]s to the `EngineActor`.
    pub engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
}

#[async_trait]
impl DerivationEngineClient for QueuedDerivationEngineClient {
    async fn reset_engine_forkchoice(&self) -> EngineClientResult<()> {
        let (result_tx, mut result_rx) = mpsc::channel(1);

        info!(target: "derivation", "Sending reset request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ResetRequest(Box::new(ResetRequest { result_tx })))
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

    async fn send_delegated_forkchoice_update(
        &self,
        update: DelegatedForkchoiceUpdate,
    ) -> EngineClientResult<()> {
        trace!(
            target: "derivation",
            safe_number = update.safe_l2.block_info.number,
            finalized_number = ?update.finalized_l2_number,
            "Sending delegated forkchoice update to engine"
        );
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessDelegatedForkchoiceUpdateRequest(Box::new(update)))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        Ok(())
    }

    async fn send_finalized_l2_block(&self, block_number: u64) -> EngineClientResult<()> {
        trace!(target: "derivation", block_number, "Sending finalized L2 block number to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(Box::new(block_number)))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        Ok(())
    }

    async fn send_safe_l2_signal(&self, signal: ConsolidateInput) -> EngineClientResult<()> {
        trace!(target: "derivation", ?signal, "Sending safe L2 signal info to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessSafeL2SignalRequest(signal))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        Ok(())
    }
}
