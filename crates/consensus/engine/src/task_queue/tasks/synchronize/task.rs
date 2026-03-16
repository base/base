//! A task for the `engine_forkchoiceUpdated` method, with no attributes.

use std::sync::Arc;

use alloy_rpc_types_engine::{INVALID_FORK_CHOICE_STATE_ERROR, PayloadStatusEnum};
use async_trait::async_trait;
use base_consensus_genesis::RollupConfig;
use derive_more::Constructor;
use tokio::time::Instant;

use crate::{
    EngineClient, EngineState, EngineTaskExt, SynchronizeTaskError, state::EngineSyncStateUpdate,
};

/// Internal task for execution layer forkchoice synchronization.
///
/// The [`SynchronizeTask`] performs `engine_forkchoiceUpdated` calls to synchronize
/// the execution layer's forkchoice state with the rollup node's view. This task
/// operates without payload attributes and is primarily used internally by other
/// engine tasks rather than being directly enqueued by users.
///
/// ## Usage Patterns
///
/// - **Internal Synchronization**: Called by [`InsertTask`], [`ConsolidateTask`], and
///   [`FinalizeTask`]
/// - **Engine Reset**: Used during engine resets to establish initial forkchoice state
/// - **Safe Head Updates**: Synchronizes safe and finalized head changes
///
/// ## Automatic Integration
///
/// Unlike the legacy `ForkchoiceTask`, forkchoice updates during block building are now
/// explicitly handled within [`BuildTask`], eliminating the need for explicit
/// forkchoice management in most user scenarios.
///
/// [`InsertTask`]: crate::InsertTask
/// [`ConsolidateTask`]: crate::ConsolidateTask
/// [`FinalizeTask`]: crate::FinalizeTask
/// [`BuildTask`]: crate::BuildTask
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
    fn check_forkchoice_updated_status(
        &self,
        state: &mut EngineState,
        status: &PayloadStatusEnum,
    ) -> Result<(), SynchronizeTaskError> {
        match status {
            PayloadStatusEnum::Valid => {
                if !state.el_sync_finished {
                    info!(
                        target: "engine",
                        "Finished execution layer sync."
                    );
                    state.el_sync_finished = true;
                }

                Ok(())
            }
            PayloadStatusEnum::Syncing => {
                // If we're not building a new payload, we're driving EL sync.
                debug!(target: "engine", "Attempting to update forkchoice state while EL syncing");
                Ok(())
            }
            s => {
                // Other codes are not expected.
                Err(SynchronizeTaskError::UnexpectedPayloadStatus(s.clone()))
            }
        }
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for SynchronizeTask<EngineClient_> {
    type Output = ();
    type Error = SynchronizeTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<Self::Output, SynchronizeTaskError> {
        // Save the current sync state so we can detect no-ops and restore on failure.
        let old_sync_state = state.sync_state;

        // Apply the sync state update to the engine state in place.
        state.sync_state.apply_update(self.state_update);

        // Check if a forkchoice update is not needed, return early.
        // A forkchoice update is not needed if...
        // 1. The engine state is not default (initial forkchoice state has been emitted), and
        // 2. The new sync state is the same as the current sync state (no changes to the sync
        //    state).
        //
        // NOTE:
        // We shouldn't retry the synchronize task there. Since the `sync_state` is only updated
        // inside the `SynchronizeTask` (except inside the ConsolidateTask, when the block is not
        // the last in the batch) - the engine will get stuck retrying the `SynchronizeTask`
        if old_sync_state != Default::default() && old_sync_state == state.sync_state {
            debug!(target: "engine", sync_state = ?state.sync_state, "No forkchoice update needed");
            return Ok(());
        }

        // Check if the head is behind the finalized head.
        if state.sync_state.unsafe_head().block_info.number
            < state.sync_state.finalized_head().block_info.number
        {
            let err = SynchronizeTaskError::FinalizedAheadOfUnsafe(
                state.sync_state.unsafe_head().block_info.number,
                state.sync_state.finalized_head().block_info.number,
            );
            state.sync_state = old_sync_state;
            return Err(err);
        }

        let fcu_time_start = Instant::now();

        // Send the forkchoice update through the input.
        let forkchoice = state.sync_state.create_forkchoice_state();

        // Handle the forkchoice update result.
        // NOTE: it doesn't matter which version we use here, because we're not sending any
        // payload attributes. The forkchoice updated call is version agnostic if no payload
        // attributes are provided.
        let response = self.client.fork_choice_updated_v3(forkchoice, None).await;

        let valid_response = match response {
            Ok(r) => r,
            Err(e) => {
                // Fatal forkchoice update error.
                let error = e
                    .as_error_resp()
                    .and_then(|e| {
                        (e.code == INVALID_FORK_CHOICE_STATE_ERROR as i64)
                            .then_some(SynchronizeTaskError::InvalidForkchoiceState)
                    })
                    .unwrap_or_else(|| SynchronizeTaskError::ForkchoiceUpdateFailed(e));

                debug!(target: "engine", error = ?error, "Unexpected forkchoice update error");

                state.sync_state = old_sync_state;
                return Err(error);
            }
        };

        if let Err(e) =
            self.check_forkchoice_updated_status(state, &valid_response.payload_status.status)
        {
            state.sync_state = old_sync_state;
            return Err(e);
        }

        let fcu_duration = fcu_time_start.elapsed();
        debug!(
            target: "engine",
            fcu_duration = ?fcu_duration,
            forkchoice = ?forkchoice,
            response = ?valid_response,
            "Forkchoice updated"
        );

        Ok(())
    }
}
