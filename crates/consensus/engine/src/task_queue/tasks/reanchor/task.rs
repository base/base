//! Task to re-anchor the unsafe head to a canonical payload.

use std::sync::Arc;

use alloy_rpc_types_engine::{ExecutionPayloadInputV2, PayloadStatusEnum};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_protocol::L2BlockInfo;
use tokio::sync::mpsc;

use crate::{
    EngineClient, EngineState, EngineTaskExt, ReanchorTaskError, SynchronizeTask,
    state::EngineSyncStateUpdate,
};

/// Result sent to callers waiting for re-anchor acknowledgement.
pub type ReanchorTaskResult = Result<L2BlockInfo, ReanchorTaskError>;

/// The task to re-anchor the unsafe head to a canonical payload.
#[derive(Debug, Clone)]
pub struct ReanchorTask<EngineClient_: EngineClient> {
    /// The engine client.
    client: Arc<EngineClient_>,
    /// The rollup config.
    rollup_config: Arc<RollupConfig>,
    /// The payload envelope.
    envelope: BaseExecutionPayloadEnvelope,
    /// Optional response channel used by callers that need acknowledgement.
    result_tx: Option<mpsc::Sender<ReanchorTaskResult>>,
}

impl<EngineClient_: EngineClient> ReanchorTask<EngineClient_> {
    /// Creates a new re-anchor task.
    pub const fn new(
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Self {
        Self { client, rollup_config, envelope, result_tx: None }
    }

    /// Creates a new re-anchor task and send acknowledgement on completion.
    pub const fn with_result(
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        result_tx: mpsc::Sender<ReanchorTaskResult>,
    ) -> Self {
        Self { client, rollup_config, envelope, result_tx: Some(result_tx) }
    }

    /// Checks the response of the `engine_newPayload` call.
    const fn check_new_payload_status(&self, status: &PayloadStatusEnum) -> bool {
        matches!(status, PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing)
    }

    async fn reanchor_payload(&self, state: &mut EngineState) -> ReanchorTaskResult {
        let parent_beacon_block_root = self.envelope.parent_beacon_block_root;
        let execution_payload = self.envelope.execution_payload.clone();
        let new_block_ref = L2BlockInfo::from_payload_and_genesis(
            execution_payload.clone(),
            parent_beacon_block_root,
            &self.rollup_config.genesis,
        )?;

        let response = match execution_payload {
            BaseExecutionPayload::V1(payload) => {
                let payload_input = ExecutionPayloadInputV2 { execution_payload: payload, withdrawals: None };
                self.client.new_payload_v2(payload_input).await
            }
            BaseExecutionPayload::V2(payload) => {
                let payload_input = ExecutionPayloadInputV2 {
                    execution_payload: payload.payload_inner,
                    withdrawals: Some(payload.withdrawals),
                };
                self.client.new_payload_v2(payload_input).await
            }
            BaseExecutionPayload::V3(payload) => {
                self.client.new_payload_v3(payload, parent_beacon_block_root.unwrap_or_default()).await
            }
            BaseExecutionPayload::V4(payload) => {
                self.client.new_payload_v4(payload, parent_beacon_block_root.unwrap_or_default()).await
            }
        };

        let response = match response {
            Ok(resp) => resp,
            Err(err) => {
                warn!(target: "engine", error = %err, "Failed to insert re-anchor payload");
                return Err(ReanchorTaskError::InsertFailed(err));
            }
        };
        if !self.check_new_payload_status(&response.status) {
            return Err(ReanchorTaskError::UnexpectedPayloadStatus(response.status));
        }

        SynchronizeTask::new(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup_config),
            EngineSyncStateUpdate { unsafe_head: Some(new_block_ref), ..Default::default() },
        )
        .execute(state)
        .await?;

        if self.result_tx.is_some() && state.sync_state.unsafe_head() != new_block_ref {
            return Err(ReanchorTaskError::ForkchoiceUpdateDidNotAdvance);
        }

        info!(
            target: "engine",
            hash = %new_block_ref.block_info.hash,
            number = new_block_ref.block_info.number,
            "Re-anchored unsafe head"
        );

        Ok(new_block_ref)
    }

    async fn send_channel_result(&self, result: ReanchorTaskResult) {
        let Some(result_tx) = &self.result_tx else { return };
        if result_tx.send(result).await.is_err() {
            warn!(target: "engine", "Sending re-anchor result failed");
        }
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for ReanchorTask<EngineClient_> {
    type Output = ();
    type Error = ReanchorTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<(), Self::Error> {
        let result = self.reanchor_payload(state).await;
        if self.result_tx.is_some() {
            self.send_channel_result(result).await;
            Ok(())
        } else {
            result.map(|_| ())
        }
    }
}
