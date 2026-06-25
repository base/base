//! Direct execution-layer build operations.

use std::{sync::Arc, time::Instant};

use alloy_rpc_types_engine::{INVALID_FORK_CHOICE_STATE_ERROR, PayloadId, PayloadStatusEnum};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_genesis::RollupConfig;
use base_protocol::AttributesWithParent;
use thiserror::Error;

use crate::{
    Engine, EngineClient, EngineForkchoiceVersion, EngineState, EngineSyncStateUpdate,
    EngineTaskError, EngineTaskErrorSeverity, Metrics,
};

/// An error that occurs during payload building within the engine.
///
/// This error type is specific to the block building process and represents failures
/// that can occur during the automatic forkchoice update phase of [`crate::Engine::build`].
/// Unlike [`BuildTaskError`], which handles higher-level build orchestration errors,
/// `EngineBuildError` focuses on low-level engine API communication failures.
///
/// ## Error Categories
///
/// - **State Validation**: Errors related to inconsistent chain state
/// - **Engine Communication**: RPC failures during forkchoice updates
/// - **Payload Validation**: Invalid payload status responses from the execution layer
///
#[derive(Debug, Error)]
pub enum EngineBuildError {
    /// The finalized head is ahead of the unsafe head.
    #[error("Finalized head is ahead of unsafe head")]
    FinalizedAheadOfUnsafe(u64, u64),
    /// The forkchoice update call to the engine api failed.
    #[error("Failed to build payload attributes in the engine. Forkchoice RPC error: {0}")]
    AttributesInsertionFailed(#[from] RpcError<TransportErrorKind>),
    /// The engine returned an invalid forkchoice state error.
    #[error("Invalid forkchoice state")]
    ForkchoiceStateInvalid,
    /// The inserted payload is invalid.
    #[error("The inserted payload is invalid: {0}")]
    InvalidPayload(String),
    /// The inserted payload status is unexpected.
    #[error("The inserted payload status is unexpected: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
    /// The payload ID is missing.
    #[error("The inserted payload ID is missing")]
    MissingPayloadId,
    /// The engine is syncing.
    #[error("The engine is syncing")]
    EngineSyncing,
}

/// An error that occurs when starting an execution-layer build.
#[derive(Debug, Error)]
pub enum BuildTaskError {
    /// An error occurred when building the payload attributes in the engine.
    #[error("An error occurred when building the payload attributes to the engine.")]
    EngineBuildError(EngineBuildError),
}

impl EngineTaskError for BuildTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::EngineBuildError(EngineBuildError::FinalizedAheadOfUnsafe(_, _)) => {
                EngineTaskErrorSeverity::Critical
            }
            // The execution layer rejected the payload attributes derived from the current
            // batch (e.g. malformed transaction bytes, invalid state transition).
            //
            // Per the derivation spec, the attributes must be dropped and the forkchoice
            // state must be left unchanged. Surfacing this as `Flush` causes the engine
            // processor to signal `FlushChannel` to the derivation pipeline, clearing the
            // poisoned batch and upstream channel state before later engine requests proceed.
            Self::EngineBuildError(EngineBuildError::InvalidPayload(_)) => {
                EngineTaskErrorSeverity::Flush
            }
            Self::EngineBuildError(EngineBuildError::AttributesInsertionFailed(_))
            | Self::EngineBuildError(EngineBuildError::UnexpectedPayloadStatus(_))
            | Self::EngineBuildError(EngineBuildError::MissingPayloadId)
            | Self::EngineBuildError(EngineBuildError::EngineSyncing) => {
                EngineTaskErrorSeverity::Temporary
            }
            Self::EngineBuildError(EngineBuildError::ForkchoiceStateInvalid) => {
                EngineTaskErrorSeverity::Reset
            }
        }
    }
}

impl Engine {
    /// Starts a block build directly against the execution layer.
    pub async fn build<EngineClient_: EngineClient + 'static>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::BUILD_TASK_LABEL));

        let result =
            Self::build_with_state(&self.state, client.as_ref(), config.as_ref(), attributes).await;

        match result {
            Ok(payload_id) => {
                Metrics::engine_task_count(Metrics::BUILD_TASK_LABEL).increment(1);
                Ok(payload_id)
            }
            Err(err) => {
                Metrics::engine_task_failure(Metrics::BUILD_TASK_LABEL, err.severity().as_label())
                    .increment(1);
                Err(err)
            }
        }
    }

    /// Starts a block build using the provided engine state.
    pub async fn build_with_state<EngineClient_: EngineClient>(
        state: &EngineState,
        engine_client: &EngineClient_,
        cfg: &RollupConfig,
        attributes_envelope: AttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        debug!(
            target: "engine_builder",
            txs = attributes_envelope
                .attributes()
                .transactions
                .as_ref()
                .map_or(0, |txs| txs.len()),
            is_deposits = attributes_envelope.is_deposits_only(),
            "Starting new build job"
        );

        let fcu_start_time = Instant::now();
        let payload_id = Self::start_build(state, engine_client, cfg, attributes_envelope).await?;
        let fcu_duration = fcu_start_time.elapsed();

        info!(
            target: "engine_builder",
            fcu_duration = ?fcu_duration,
            "block build started"
        );

        Ok(payload_id)
    }

    /// Validates a forkchoice update status returned while starting a build.
    pub fn validate_forkchoice_status(status: PayloadStatusEnum) -> Result<(), BuildTaskError> {
        match status {
            PayloadStatusEnum::Valid => Ok(()),
            PayloadStatusEnum::Invalid { validation_error } => {
                error!(target: "engine_builder", error = %validation_error, "Forkchoice update failed");
                Err(BuildTaskError::EngineBuildError(EngineBuildError::InvalidPayload(
                    validation_error,
                )))
            }
            PayloadStatusEnum::Syncing => {
                warn!(target: "engine_builder", "Forkchoice update failed temporarily: EL is syncing");
                Err(BuildTaskError::EngineBuildError(EngineBuildError::EngineSyncing))
            }
            PayloadStatusEnum::Accepted => Err(BuildTaskError::EngineBuildError(
                EngineBuildError::UnexpectedPayloadStatus(status),
            )),
        }
    }

    /// Sends the forkchoice update that starts an execution-layer build job.
    pub async fn start_build<EngineClient_: EngineClient>(
        state: &EngineState,
        engine_client: &EngineClient_,
        cfg: &RollupConfig,
        attributes_envelope: AttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        if state.sync_state.unsafe_head().block_info.number
            < state.sync_state.finalized_head().block_info.number
        {
            return Err(BuildTaskError::EngineBuildError(
                EngineBuildError::FinalizedAheadOfUnsafe(
                    state.sync_state.unsafe_head().block_info.number,
                    state.sync_state.finalized_head().block_info.number,
                ),
            ));
        }

        let new_forkchoice = state
            .sync_state
            .apply_update(EngineSyncStateUpdate {
                unsafe_head: Some(attributes_envelope.parent),
                ..Default::default()
            })
            .create_forkchoice_state();

        let forkchoice_version = EngineForkchoiceVersion::from_cfg(
            cfg,
            attributes_envelope.attributes.payload_attributes.timestamp,
        );
        let attrs = attributes_envelope.attributes;
        let update = match forkchoice_version {
            EngineForkchoiceVersion::V3 => {
                engine_client.fork_choice_updated_v3(new_forkchoice, Some(attrs)).await
            }
            EngineForkchoiceVersion::V2 => {
                engine_client.fork_choice_updated_v2(new_forkchoice, Some(attrs)).await
            }
        }
        .map_err(|e| {
            error!(target: "engine_builder", error = %e, "Forkchoice update failed");
            let error = e
                .as_error_resp()
                .and_then(|e| {
                    (e.code == INVALID_FORK_CHOICE_STATE_ERROR as i64)
                        .then_some(EngineBuildError::ForkchoiceStateInvalid)
                })
                .unwrap_or_else(|| EngineBuildError::AttributesInsertionFailed(e));

            BuildTaskError::EngineBuildError(error)
        })?;

        Self::validate_forkchoice_status(update.payload_status.status)?;

        debug!(
            target: "engine_builder",
            unsafe_hash = new_forkchoice.head_block_hash.to_string(),
            safe_hash = new_forkchoice.safe_block_hash.to_string(),
            finalized_hash = new_forkchoice.finalized_block_hash.to_string(),
            "Forkchoice update with attributes successful"
        );

        update
            .payload_id
            .ok_or(BuildTaskError::EngineBuildError(EngineBuildError::MissingPayloadId))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use base_common_genesis::RollupConfig;
    use tokio::{sync::watch, time::timeout};

    use super::*;
    use crate::test_utils::{
        TestAttributesBuilder, TestEngineStateBuilder, test_block_info, test_engine_client_builder,
    };

    fn invalid_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Invalid {
                    validation_error: "malformed transaction".into(),
                },
                latest_valid_hash: Some(B256::with_last_byte(2)),
            },
            payload_id: None,
        }
    }

    /// `InvalidPayload` must surface `Flush` so the engine processor flushes
    /// the derivation channel and carries the EL's `validation_error` so operators
    /// can identify which batch poisoned the pipeline.
    #[test]
    fn invalid_payload_is_flush_and_preserves_validation_error() {
        let err = BuildTaskError::EngineBuildError(EngineBuildError::InvalidPayload(
            "malformed transaction at index 3".to_string(),
        ));

        assert_eq!(err.severity(), EngineTaskErrorSeverity::Flush);
        assert!(
            format!("{err:?}").contains("malformed transaction at index 3"),
            "Debug must surface the EL validation error, got: {err:?}",
        );
    }

    #[tokio::test]
    async fn direct_build_invalid_payload_returns_flush() {
        let parent_block = test_block_info(0);
        let unsafe_block = test_block_info(1);
        let attributes_timestamp = unsafe_block.block_info.timestamp;

        let mut cfg = RollupConfig::default();
        cfg.upgrades.ecotone_time = Some(attributes_timestamp);

        let client = Arc::new(
            test_engine_client_builder()
                .with_fork_choice_updated_v3_response(invalid_fcu())
                .build(),
        );
        let cfg = Arc::new(cfg);

        let attributes = TestAttributesBuilder::new()
            .with_parent(parent_block)
            .with_timestamp(attributes_timestamp)
            .build();

        let initial_state = TestEngineStateBuilder::new()
            .with_unsafe_head(unsafe_block)
            .with_safe_head(parent_block)
            .with_finalized_head(parent_block)
            .build();

        let (state_tx, _state_rx) = watch::channel(initial_state);
        let mut engine = Engine::new(initial_state, state_tx);

        let err = engine
            .build(Arc::clone(&client), Arc::clone(&cfg), attributes)
            .await
            .expect_err("invalid FCU must fail build");
        assert_eq!(err.severity(), EngineTaskErrorSeverity::Flush);
        assert_eq!(engine.state().sync_state, initial_state.sync_state);
    }

    #[tokio::test]
    async fn build_returns_temporary_error_without_retrying() {
        let parent_block = test_block_info(0);
        let unsafe_block = test_block_info(1);
        let attributes_timestamp = unsafe_block.block_info.timestamp;

        let mut cfg = RollupConfig::default();
        cfg.upgrades.ecotone_time = Some(attributes_timestamp);

        let attributes = TestAttributesBuilder::new()
            .with_parent(parent_block)
            .with_timestamp(attributes_timestamp)
            .build();
        let initial_state = TestEngineStateBuilder::new()
            .with_unsafe_head(unsafe_block)
            .with_safe_head(parent_block)
            .with_finalized_head(parent_block)
            .build();
        let (state_tx, _state_rx) = watch::channel(initial_state);
        let mut engine = Engine::new(initial_state, state_tx);

        let result = timeout(
            Duration::from_millis(100),
            engine.build(Arc::new(test_engine_client_builder().build()), Arc::new(cfg), attributes),
        )
        .await
        .expect("build should return the first temporary error instead of retrying forever");

        assert!(matches!(
            result,
            Err(BuildTaskError::EngineBuildError(EngineBuildError::AttributesInsertionFailed(_),)),
        ));
    }
}
