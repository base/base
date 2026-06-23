//! Forkchoice synchronization operation.

use std::sync::Arc;

use alloy_rpc_types_engine::{INVALID_FORK_CHOICE_STATE_ERROR, PayloadStatusEnum};
use alloy_transport::{RpcError, TransportErrorKind};
use base_common_genesis::RollupConfig;
use derive_more::Constructor;
use thiserror::Error;
use tokio::time::Instant;

use crate::{
    EngineClient, EngineState, EngineSyncStateUpdate, EngineTaskError, EngineTaskErrorSeverity,
};

/// An error that occurs when running the [`SynchronizeTask`].
#[derive(Debug, Error)]
pub enum SynchronizeTaskError {
    /// The forkchoice update call to the engine api failed.
    #[error("Forkchoice update engine api call failed due to an RPC error: {0}")]
    ForkchoiceUpdateFailed(RpcError<TransportErrorKind>),
    /// The finalized head is behind the unsafe head.
    #[error("Invalid forkchoice state: unsafe head {0} is ahead of finalized head {1}")]
    FinalizedAheadOfUnsafe(u64, u64),
    /// The forkchoice state is invalid.
    #[error("Invalid forkchoice state")]
    InvalidForkchoiceState,
    /// The payload status is unexpected.
    #[error("Unexpected payload status: {0}")]
    UnexpectedPayloadStatus(PayloadStatusEnum),
}

impl EngineTaskError for SynchronizeTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::FinalizedAheadOfUnsafe(_, _) => EngineTaskErrorSeverity::Critical,
            Self::ForkchoiceUpdateFailed(_) | Self::UnexpectedPayloadStatus(_) => {
                EngineTaskErrorSeverity::Temporary
            }
            Self::InvalidForkchoiceState => EngineTaskErrorSeverity::Reset,
        }
    }
}

/// Internal task for execution layer forkchoice synchronization.
///
/// The [`SynchronizeTask`] performs `engine_forkchoiceUpdated` calls to synchronize
/// the execution layer's forkchoice state with the rollup node's view. This task
/// operates without payload attributes and is primarily used internally by other
/// direct engine operations rather than being directly enqueued by users.
///
/// ## Usage Patterns
///
/// - **Internal Synchronization**: Called by direct insert/consolidate/finalize processing
/// - **Engine Reset**: Used during engine resets to establish initial forkchoice state
/// - **Safe Head Updates**: Synchronizes safe and finalized head changes
///
/// ## Automatic Integration
///
/// Unlike the legacy `ForkchoiceTask`, forkchoice updates during block building are now
/// explicitly handled within direct build processing, eliminating the need for explicit
/// forkchoice management in most user scenarios.
#[derive(Debug, Clone, Constructor)]
pub struct SynchronizeTask<EngineClient_: EngineClient> {
    /// The engine client.
    pub client: Arc<EngineClient_>,
    /// The rollup config.
    pub rollup: Arc<RollupConfig>,
    /// The sync state update to apply to the engine state.
    pub state_update: EngineSyncStateUpdate,
}

impl<EngineClient_: EngineClient> SynchronizeTask<EngineClient_> {
    /// Checks the response of the `engine_forkchoiceUpdated` call, and updates the sync status if
    /// necessary.
    ///
    /// Returns `true` if the EL confirmed the forkchoice (`Valid`), meaning the caller
    /// should apply the proposed sync-state update. Returns `false` for `Syncing`,
    /// indicating the EL accepted the hint but has **not** canonicalised the head - the
    /// caller must leave `state.sync_state` unchanged so that the node's view of the
    /// chain does not advance beyond what the EL can actually serve.
    fn check_forkchoice_updated_status(
        &self,
        state: &mut EngineState,
        status: &PayloadStatusEnum,
    ) -> Result<bool, SynchronizeTaskError> {
        match status {
            PayloadStatusEnum::Valid => {
                if !state.el_sync_finished {
                    info!(
                        target: "engine",
                        "Finished execution layer sync."
                    );
                    state.el_sync_finished = true;
                }

                Ok(true)
            }
            PayloadStatusEnum::Syncing => {
                // The EL stored the block but cannot validate it yet (e.g. missing parent). We
                // intentionally do not apply the sync-state update, so unsafe_head stays at the
                // last confirmed value.
                debug!(target: "engine", "Forkchoice update returned Syncing; state not advanced");
                Ok(false)
            }
            status => Err(SynchronizeTaskError::UnexpectedPayloadStatus(status.clone())),
        }
    }

    /// Applies the forkchoice update to the execution layer and engine state.
    pub async fn execute(&self, state: &mut EngineState) -> Result<(), SynchronizeTaskError> {
        let new_sync_state = state.sync_state.apply_update(self.state_update);

        // A forkchoice update is unnecessary once an initial forkchoice state has been emitted and
        // this update does not change any sync-state labels.
        if state.sync_state != Default::default() && state.sync_state == new_sync_state {
            debug!(target: "engine", ?new_sync_state, "No forkchoice update needed");
            return Ok(());
        }

        if new_sync_state.unsafe_head().block_info.number
            < new_sync_state.finalized_head().block_info.number
        {
            return Err(SynchronizeTaskError::FinalizedAheadOfUnsafe(
                new_sync_state.unsafe_head().block_info.number,
                new_sync_state.finalized_head().block_info.number,
            ));
        }

        let fcu_time_start = Instant::now();
        let forkchoice = new_sync_state.create_forkchoice_state();

        // The no-attributes forkchoice update is version-agnostic, so V3 is sufficient here.
        let response = self.client.fork_choice_updated_v3(forkchoice, None).await;

        let valid_response = response.map_err(|e| {
            let error = e
                .as_error_resp()
                .and_then(|e| {
                    (e.code == INVALID_FORK_CHOICE_STATE_ERROR as i64)
                        .then_some(SynchronizeTaskError::InvalidForkchoiceState)
                })
                .unwrap_or_else(|| SynchronizeTaskError::ForkchoiceUpdateFailed(e));

            debug!(target: "engine", error = ?error, "Unexpected forkchoice update error");

            error
        })?;

        let confirmed =
            self.check_forkchoice_updated_status(state, &valid_response.payload_status.status)?;

        if confirmed {
            state.sync_state = new_sync_state;
        }

        let fcu_duration = fcu_time_start.elapsed();
        debug!(
            target: "engine",
            fcu_duration = ?fcu_duration,
            forkchoice = ?forkchoice,
            ?confirmed,
            response = ?valid_response,
            "Forkchoice updated"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use base_common_genesis::RollupConfig;

    use super::*;
    use crate::test_utils::{TestEngineStateBuilder, test_block_info, test_engine_client_builder};

    fn syncing_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Syncing,
                latest_valid_hash: None,
            },
            payload_id: None,
        }
    }

    fn valid_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            },
            payload_id: None,
        }
    }

    #[tokio::test]
    async fn valid_response_advances_sync_state() {
        let head = test_block_info(100);
        let cfg = Arc::new(RollupConfig::default());
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
        );

        let mut state = TestEngineStateBuilder::new().build();

        let task = SynchronizeTask::new(
            client,
            cfg,
            EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() },
        );

        task.execute(&mut state).await.expect("should succeed");

        assert_eq!(
            state.sync_state.unsafe_head().block_info.number,
            100,
            "unsafe_head must advance on Valid response",
        );
        assert!(state.el_sync_finished, "el_sync_finished must be true after Valid");
    }

    #[tokio::test]
    async fn syncing_response_does_not_advance_sync_state() {
        let head = test_block_info(100);
        let cfg = Arc::new(RollupConfig::default());
        let client = Arc::new(
            test_engine_client_builder()
                .with_fork_choice_updated_v3_response(syncing_fcu())
                .build(),
        );

        let mut state = TestEngineStateBuilder::new().with_el_sync_finished(false).build();
        let original_unsafe = state.sync_state.unsafe_head();

        let task = SynchronizeTask::new(
            client,
            cfg,
            EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() },
        );

        task.execute(&mut state).await.expect("should succeed");

        assert_eq!(
            state.sync_state.unsafe_head(),
            original_unsafe,
            "unsafe_head must not advance on Syncing response",
        );
        assert!(!state.el_sync_finished, "el_sync_finished must remain false after Syncing");
    }

    #[tokio::test]
    async fn syncing_then_valid_advances_state_on_second_call() {
        let head_a = test_block_info(100);
        let head_b = test_block_info(101);
        let cfg = Arc::new(RollupConfig::default());

        let client = Arc::new(
            test_engine_client_builder()
                .with_fork_choice_updated_v3_response(syncing_fcu())
                .build(),
        );

        let mut state = TestEngineStateBuilder::new().with_el_sync_finished(false).build();

        let task = SynchronizeTask::new(
            Arc::clone(&client),
            Arc::clone(&cfg),
            EngineSyncStateUpdate { unsafe_head: Some(head_a), ..Default::default() },
        );
        task.execute(&mut state).await.expect("should succeed");
        assert_eq!(state.sync_state.unsafe_head().block_info.number, 0);
        assert!(!state.el_sync_finished);

        client.set_fork_choice_updated_v3_response(valid_fcu()).await;

        let task = SynchronizeTask::new(
            Arc::clone(&client),
            Arc::clone(&cfg),
            EngineSyncStateUpdate { unsafe_head: Some(head_b), ..Default::default() },
        );
        task.execute(&mut state).await.expect("should succeed");
        assert_eq!(
            state.sync_state.unsafe_head().block_info.number,
            101,
            "unsafe_head must advance after Valid",
        );
        assert!(state.el_sync_finished);
    }
}
