use std::sync::Arc;

use async_trait::async_trait;
use base_common_types_chain::BaseBlock;
use base_common_types_payload::{BaseExecutionPayloadEnvelope, PayloadId};
use base_consensus_batch::{AttributesWithParent, L2BlockInfo};
use base_consensus_driver::{ResetReason, SequencerEngineClient};
use tokio::sync::mpsc;

use super::ExecutionPayloadConverter;

/// Sequencer engine client adapter that reports inserted blocks back to the harness driver.
///
/// Wraps any [`SequencerEngineClient`] backend — the in-memory
/// [`ActionEngineClient`](crate::action_fixtures::ActionEngineClient) or the production-builder-backed
/// [`BuilderBackedEngineClient`](crate::action_fixtures::BuilderBackedEngineClient) — so the harness's production
/// `SequencerActor` can drive either through the same seam.
#[derive(Clone)]
pub struct ActionSequencerEngineClient {
    inner: Arc<dyn SequencerEngineClient>,
    inserted_tx: mpsc::Sender<(BaseBlock, L2BlockInfo)>,
}

impl core::fmt::Debug for ActionSequencerEngineClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ActionSequencerEngineClient").finish_non_exhaustive()
    }
}

impl ActionSequencerEngineClient {
    /// Create a new engine client adapter over any sequencer engine backend.
    pub fn new(
        inner: Arc<dyn SequencerEngineClient>,
        inserted_tx: mpsc::Sender<(BaseBlock, L2BlockInfo)>,
    ) -> Self {
        Self { inner, inserted_tx }
    }
}

#[async_trait]
impl SequencerEngineClient for ActionSequencerEngineClient {
    async fn reset_engine_forkchoice(
        &self,
        reason: ResetReason,
    ) -> Result<(), base_consensus_driver::EngineClientError> {
        self.inner.reset_engine_forkchoice(reason).await
    }

    async fn start_building(
        &self,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, base_consensus_driver::EngineClientError> {
        self.inner.start_building(attributes).await
    }

    async fn end_building(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, base_consensus_driver::EngineClientError> {
        self.inner.end_building(payload_id, attributes).await
    }

    async fn append_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<L2BlockInfo, base_consensus_driver::EngineClientError> {
        let block = ExecutionPayloadConverter::block_from_envelope(&payload)
            .map_err(|e| base_consensus_driver::EngineClientError::ResponseError(e.to_string()))?;
        let inserted_head = self.inner.append_payload(payload).await?;
        let _ = self.inserted_tx.send((block, inserted_head)).await;
        Ok(inserted_head)
    }

    async fn get_unsafe_head(
        &self,
    ) -> Result<L2BlockInfo, base_consensus_driver::EngineClientError> {
        self.inner.get_unsafe_head().await
    }

    async fn el_sync_finished(&self) -> Result<bool, base_consensus_driver::EngineClientError> {
        self.inner.el_sync_finished().await
    }
}
