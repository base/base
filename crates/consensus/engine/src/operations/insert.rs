//! Direct payload insertion operations.

use std::{sync::Arc, time::Instant};

use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayloadInputV2, PayloadStatusEnum, PraguePayloadFields,
};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    BasePayloadError,
};
use base_protocol::{FromBlockError, L2BlockInfo};
use thiserror::Error;

use crate::{
    Engine, EngineClient, EngineState, EngineSyncStateUpdate, EngineTaskError,
    EngineTaskErrorSeverity, Metrics, SynchronizeTask, SynchronizeTaskError,
};

/// An error that occurs when inserting a payload into the execution engine.
#[derive(Debug, Error)]
pub enum InsertTaskError {
    /// Error converting a payload into a block.
    #[error(transparent)]
    FromBlockError(#[from] BasePayloadError),
    /// Failed to insert new payload.
    #[error("Failed to insert new payload: {0}")]
    InsertFailed(RpcError<TransportErrorKind>),
    /// Unexpected payload status
    #[error("Unexpected payload status: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
    /// Error converting the payload + chain genesis into an L2 block info.
    #[error(transparent)]
    L2BlockInfoConstruction(#[from] FromBlockError),
    /// The forkchoice update call to consolidate the block into the engine state failed.
    #[error(transparent)]
    ForkchoiceUpdateFailed(#[from] SynchronizeTaskError),
    /// The forkchoice update completed without advancing the unsafe head to the inserted payload.
    #[error("Forkchoice update did not advance to the inserted payload")]
    ForkchoiceUpdateDidNotAdvance,
}

impl EngineTaskError for InsertTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::FromBlockError(_) | Self::L2BlockInfoConstruction(_) => {
                EngineTaskErrorSeverity::Critical
            }
            Self::InsertFailed(_)
            | Self::UnexpectedPayloadStatus(_)
            | Self::ForkchoiceUpdateDidNotAdvance => EngineTaskErrorSeverity::Temporary,
            Self::ForkchoiceUpdateFailed(inner) => inner.severity(),
        }
    }
}

/// Result sent to callers waiting for payload insertion acknowledgement.
pub type InsertTaskResult = Result<L2BlockInfo, InsertTaskError>;

/// Whether inserting a payload should advance the safe head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPayloadSafety {
    /// Insert an unsafe payload.
    Unsafe,
    /// Insert a payload that is already safe.
    Safe,
}

impl InsertPayloadSafety {
    /// Returns true if this insert should advance the safe head.
    pub const fn advances_safe_head(self) -> bool {
        matches!(self, Self::Safe)
    }

    /// Returns the label used for structured logs.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Unsafe => "unsafe",
            Self::Safe => "safe",
        }
    }
}

impl Engine {
    /// Inserts an external unsafe payload, retrying temporary failures.
    pub async fn insert_unsafe_payload<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> InsertTaskResult
    where
        EngineClient_: EngineClient + 'static,
    {
        self.insert_payload_with_retry_inner(
            client,
            config,
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
    }

    /// Inserts a local sequencer unsafe payload once and returns the insertion result.
    pub async fn insert_local_unsafe_payload<EngineClient_: EngineClient>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> InsertTaskResult {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::INSERT_TASK_LABEL));

        let result = Self::insert_payload_with_state(
            &mut self.state,
            client,
            config,
            envelope,
            InsertPayloadSafety::Unsafe,
            true,
        )
        .await;

        self.state_sender.send_replace(self.state);
        Metrics::engine_task_count(Metrics::INSERT_TASK_LABEL).increment(1);
        if let Err(err) = &result {
            Metrics::engine_task_failure(Metrics::INSERT_TASK_LABEL, err.severity().as_label())
                .increment(1);
        }

        result
    }

    /// Inserts a payload and retries temporary failures.
    pub async fn insert_payload_with_retry<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        payload_safety: InsertPayloadSafety,
    ) -> InsertTaskResult
    where
        EngineClient_: EngineClient + 'static,
    {
        self.insert_payload_with_retry_inner(client, config, envelope, payload_safety, false).await
    }

    /// Inserts a payload and retries temporary failures.
    pub async fn insert_payload_with_retry_inner<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        payload_safety: InsertPayloadSafety,
        require_unsafe_head_advance: bool,
    ) -> InsertTaskResult
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::INSERT_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            let envelope = envelope.clone();
            Box::pin(async move {
                Self::insert_payload_with_state(
                    state,
                    client,
                    config,
                    envelope,
                    payload_safety,
                    require_unsafe_head_advance,
                )
                .await
            })
        })
        .await
    }

    /// Inserts a payload into the execution engine using the provided state.
    pub async fn insert_payload_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        payload_safety: InsertPayloadSafety,
        require_unsafe_head_advance: bool,
    ) -> InsertTaskResult {
        let total_insert_start = Instant::now();
        let BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload } = envelope;
        let parent_beacon_block_root = parent_beacon_block_root.unwrap_or_default();
        let block: BaseBlock = match &execution_payload {
            BaseExecutionPayload::V1(payload) => BaseExecutionPayload::V1(payload.clone())
                .try_into_block()
                .map_err(InsertTaskError::FromBlockError)?,
            BaseExecutionPayload::V2(payload) => BaseExecutionPayload::V2(payload.clone())
                .try_into_block()
                .map_err(InsertTaskError::FromBlockError)?,
            BaseExecutionPayload::V3(payload) => BaseExecutionPayload::V3(payload.clone())
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v3(
                    CancunPayloadFields::new(parent_beacon_block_root, vec![]),
                ))
                .map_err(InsertTaskError::FromBlockError)?,
            BaseExecutionPayload::V4(payload) => BaseExecutionPayload::V4(payload.clone())
                .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v4(
                    CancunPayloadFields::new(parent_beacon_block_root, vec![]),
                    PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                ))
                .map_err(InsertTaskError::FromBlockError)?,
        };

        let advances_safe_head = payload_safety.advances_safe_head();
        let new_block_ref = L2BlockInfo::from_block_and_genesis(&block, &rollup_config.genesis)
            .map_err(InsertTaskError::L2BlockInfoConstruction)?;

        if !Self::is_unsafe_payload_applicable(state, payload_safety, &new_block_ref) {
            return Ok(state.sync_state.unsafe_head());
        }

        let new_payload_rpc_start = Instant::now();
        let response = match execution_payload {
            BaseExecutionPayload::V1(payload) => {
                let payload_input =
                    ExecutionPayloadInputV2 { execution_payload: payload, withdrawals: None };
                client.new_payload_v2(payload_input).await
            }
            BaseExecutionPayload::V2(payload) => {
                let payload_input = ExecutionPayloadInputV2 {
                    execution_payload: payload.payload_inner,
                    withdrawals: Some(payload.withdrawals),
                };
                client.new_payload_v2(payload_input).await
            }
            BaseExecutionPayload::V3(payload) => {
                client.new_payload_v3(payload, parent_beacon_block_root).await
            }
            BaseExecutionPayload::V4(payload) => {
                client.new_payload_v4(payload, parent_beacon_block_root).await
            }
        };

        let response = match response {
            Ok(resp) => resp,
            Err(e) => {
                warn!(
                    target: "engine",
                    error = %e,
                    payload_safety = payload_safety.as_label(),
                    "Failed to insert new payload"
                );
                return Err(InsertTaskError::InsertFailed(e));
            }
        };
        if !Self::check_new_payload_status(&response.status) {
            return Err(InsertTaskError::UnexpectedPayloadStatus(response.status));
        }
        let new_payload_rpc_duration = new_payload_rpc_start.elapsed();

        SynchronizeTask::new(
            Arc::clone(&client),
            Arc::clone(&rollup_config),
            EngineSyncStateUpdate {
                unsafe_head: Some(new_block_ref),
                local_safe_head: advances_safe_head.then_some(new_block_ref),
                safe_head: advances_safe_head.then_some(new_block_ref),
                ..Default::default()
            },
        )
        .execute(state)
        .await?;

        if require_unsafe_head_advance && state.sync_state.unsafe_head() != new_block_ref {
            return Err(InsertTaskError::ForkchoiceUpdateDidNotAdvance);
        }

        let total_insert_duration = total_insert_start.elapsed();

        info!(
            target: "engine",
            hash = %new_block_ref.block_info.hash,
            number = new_block_ref.block_info.number,
            payload_safety = payload_safety.as_label(),
            total_insert_duration = ?total_insert_duration,
            new_payload_rpc_duration = ?new_payload_rpc_duration,
            "Inserted new payload"
        );

        Ok(new_block_ref)
    }

    /// Returns whether an unsafe payload should be imported into the execution layer.
    pub fn is_unsafe_payload_applicable(
        state: &EngineState,
        payload_safety: InsertPayloadSafety,
        new_unsafe_ref: &L2BlockInfo,
    ) -> bool {
        if payload_safety.advances_safe_head() {
            return true;
        }

        let unsafe_head = state.sync_state.unsafe_head();
        if new_unsafe_ref.block_info.hash == unsafe_head.block_info.hash {
            debug!(
                target: "engine",
                hash = %new_unsafe_ref.block_info.hash,
                number = new_unsafe_ref.block_info.number,
                "Skipping already processed unsafe payload"
            );
            return false;
        }

        if new_unsafe_ref.block_info.number <= unsafe_head.block_info.number {
            info!(
                target: "engine",
                hash = %new_unsafe_ref.block_info.hash,
                number = new_unsafe_ref.block_info.number,
                unsafe_hash = %unsafe_head.block_info.hash,
                unsafe_number = unsafe_head.block_info.number,
                "Skipping unsafe payload older than current unsafe head"
            );
            return false;
        }

        if new_unsafe_ref.block_info.number == unsafe_head.block_info.number.saturating_add(1)
            && new_unsafe_ref.block_info.parent_hash != unsafe_head.block_info.hash
        {
            info!(
                target: "engine",
                hash = %new_unsafe_ref.block_info.hash,
                number = new_unsafe_ref.block_info.number,
                parent_hash = %new_unsafe_ref.block_info.parent_hash,
                unsafe_hash = %unsafe_head.block_info.hash,
                unsafe_number = unsafe_head.block_info.number,
                "Skipping unsafe payload that does not build onto current unsafe head"
            );
            return false;
        }

        true
    }

    /// Checks the response of the `engine_newPayload` call.
    pub const fn check_new_payload_status(status: &PayloadStatusEnum) -> bool {
        matches!(status, PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing)
    }
}
