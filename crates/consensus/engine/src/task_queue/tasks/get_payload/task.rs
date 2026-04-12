//! A task for fetching a sealed payload from the engine without inserting it.
use std::sync::Arc;

use alloy_rpc_types_engine::{ExecutionPayload, PayloadId};
use async_trait::async_trait;
use base_alloy_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_consensus_genesis::RollupConfig;
use base_protocol::AttributesWithParent;
use derive_more::Constructor;
use tokio::sync::mpsc;

use super::super::SealTaskError;
use crate::{EngineClient, EngineGetPayloadVersion, EngineState, EngineTaskExt, Metrics};

/// Task for fetching a sealed payload from the engine without inserting it.
///
/// Unlike [`SealTask`], this task only performs the `engine_getPayload` step and
/// sends the resulting [`BaseExecutionPayloadEnvelope`] back to the caller. It does
/// NOT import the payload into the engine (no `new_payload` or FCU calls).
///
/// This enables the sequencer to commit to the conductor before engine insertion.
///
/// [`SealTask`]: crate::SealTask
#[derive(Debug, Clone, Constructor)]
pub struct GetPayloadTask<EngineClient_: EngineClient> {
    /// The engine API client.
    pub engine: Arc<EngineClient_>,
    /// The [`RollupConfig`].
    pub cfg: Arc<RollupConfig>,
    /// The [`PayloadId`] to fetch.
    pub payload_id: PayloadId,
    /// The [`AttributesWithParent`] used for version selection and parent validation.
    pub attributes: AttributesWithParent,
    /// An optional sender to convey the sealed [`BaseExecutionPayloadEnvelope`]
    /// or the [`SealTaskError`] that occurred during fetching.
    pub result_tx: Option<mpsc::Sender<Result<BaseExecutionPayloadEnvelope, SealTaskError>>>,
}

impl<EngineClient_: EngineClient> GetPayloadTask<EngineClient_> {
    /// Fetches the execution payload from the EL, returning the execution envelope.
    ///
    /// This is the same version-dispatch logic as [`SealTask::seal_payload`] but without
    /// any insertion step.
    async fn get_payload(
        &self,
        cfg: &RollupConfig,
        engine: &EngineClient_,
        payload_id: PayloadId,
        payload_attrs: &AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        let payload_timestamp = payload_attrs.attributes().payload_attributes.timestamp;

        debug!(
            target: "engine",
            payload_id = payload_id.to_string(),
            l2_time = payload_timestamp,
            "Fetching payload"
        );

        let get_payload_version = EngineGetPayloadVersion::from_cfg(cfg, payload_timestamp);
        let payload_envelope = match get_payload_version {
            EngineGetPayloadVersion::V5 => {
                let payload = engine.get_payload_v5(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: payload_attrs
                        .attributes()
                        .payload_attributes
                        .parent_beacon_block_root,
                    execution_payload: BaseExecutionPayload::V4(payload.execution_payload),
                }
            }
            EngineGetPayloadVersion::V4 => {
                let payload = engine.get_payload_v4(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: Some(payload.parent_beacon_block_root),
                    execution_payload: BaseExecutionPayload::V4(payload.execution_payload),
                }
            }
            EngineGetPayloadVersion::V3 => {
                let payload = engine.get_payload_v3(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: Some(payload.parent_beacon_block_root),
                    execution_payload: BaseExecutionPayload::V3(payload.execution_payload),
                }
            }
            EngineGetPayloadVersion::V2 => {
                let payload = engine.get_payload_v2(payload_id).await.map_err(|e| {
                    error!(target: "engine", error = %e, "Payload fetch failed");
                    SealTaskError::GetPayloadFailed(e)
                })?;

                BaseExecutionPayloadEnvelope {
                    parent_beacon_block_root: None,
                    execution_payload: match payload.execution_payload.into_payload() {
                        ExecutionPayload::V1(payload) => BaseExecutionPayload::V1(payload),
                        ExecutionPayload::V2(payload) => BaseExecutionPayload::V2(payload),
                        _ => unreachable!("the response should be a V1 or V2 payload"),
                    },
                }
            }
        };

        Ok(payload_envelope)
    }

    /// Sends the provided result via the `result_tx` sender if one exists, returning the
    /// appropriate error if it does not.
    async fn send_channel_result_or_get_error(
        &self,
        res: Result<BaseExecutionPayloadEnvelope, SealTaskError>,
    ) -> Result<(), SealTaskError> {
        if let Some(tx) = &self.result_tx {
            tx.send(res).await.map_err(|e| SealTaskError::MpscSend(Box::new(e)))?;
        } else if let Err(x) = res {
            return Err(x);
        }

        Ok(())
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for GetPayloadTask<EngineClient_> {
    type Output = ();

    type Error = SealTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<(), SealTaskError> {
        debug!(
            target: "engine",
            "Starting new get-payload job"
        );

        let unsafe_block_info = state.sync_state.unsafe_head().block_info;
        let parent_block_info = self.attributes.parent.block_info;

        let res = if unsafe_block_info.hash != parent_block_info.hash
            || unsafe_block_info.number != parent_block_info.number
        {
            error!(
                target: "engine",
                unsafe_block_info = ?unsafe_block_info,
                parent_block_info = ?parent_block_info,
                "GetPayload attributes parent does not match unsafe head, returning rebuild error"
            );
            Metrics::sequencer_unsafe_head_changed_total().increment(1);
            Err(SealTaskError::UnsafeHeadChangedSinceBuild)
        } else {
            self.get_payload(&self.cfg, &self.engine, self.payload_id, &self.attributes).await
        };

        self.send_channel_result_or_get_error(res).await?;

        Ok(())
    }
}
