use std::sync::Arc;

use alloy_rpc_types_engine::PayloadId;
use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_node::SequencerEngineClient;
use base_protocol::{AttributesWithParent, L2BlockInfo};
use tokio::sync::mpsc;

use super::ExecutionPayloadConverter;
use crate::ActionEngineClient;

/// Sequencer engine client adapter that reports inserted blocks back to the harness driver.
#[derive(Debug, Clone)]
pub struct ActionSequencerEngineClient {
    inner: Arc<ActionEngineClient>,
    inserted_tx: mpsc::Sender<(BaseBlock, L2BlockInfo)>,
}

impl ActionSequencerEngineClient {
    /// Create a new engine client adapter.
    pub const fn new(
        inner: Arc<ActionEngineClient>,
        inserted_tx: mpsc::Sender<(BaseBlock, L2BlockInfo)>,
    ) -> Self {
        Self { inner, inserted_tx }
    }
}

#[async_trait]
impl SequencerEngineClient for ActionSequencerEngineClient {
    async fn reset_engine_forkchoice(&self) -> Result<(), base_consensus_node::EngineClientError> {
        self.inner.reset_engine_forkchoice().await
    }

    async fn start_build_block(
        &self,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, base_consensus_node::EngineClientError> {
        self.inner.start_build_block(attributes).await
    }

    async fn get_sealed_payload(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, base_consensus_node::EngineClientError> {
        self.inner.get_sealed_payload(payload_id, attributes).await
    }

    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<L2BlockInfo, base_consensus_node::EngineClientError> {
        let block = ExecutionPayloadConverter::block_from_envelope(&payload)
            .map_err(|e| base_consensus_node::EngineClientError::ResponseError(e.to_string()))?;
        let inserted_head = self.inner.insert_unsafe_payload(payload).await?;
        let _ = self.inserted_tx.send((block, inserted_head)).await;
        Ok(inserted_head)
    }

    async fn get_unsafe_head(&self) -> Result<L2BlockInfo, base_consensus_node::EngineClientError> {
        self.inner.get_unsafe_head().await
    }
}
