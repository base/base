//! A task for the `engine_forkchoiceUpdated` method, with no attributes.

use std::sync::Arc;

use async_trait::async_trait;
use base_common_chain_config::RollupConfig;
use base_consensus_batch::L2BlockInfo;
use tokio::time::Instant;

use crate::engine::{
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
/// explicitly handled within direct build processing, eliminating the need for explicit
/// forkchoice management in most user scenarios.
///
/// [`InsertTask`]: crate::engine::InsertTask
/// [`ConsolidateTask`]: crate::engine::ConsolidateTask  
/// [`FinalizeTask`]: crate::engine::FinalizeTask
#[derive(Debug, Clone)]
pub struct SynchronizeTask<EngineClient_: EngineClient> {
    /// The engine client.
    pub client: Arc<EngineClient_>,
    /// The rollup config.
    pub rollup: Arc<RollupConfig>,
    /// The sync state update to apply to the engine state.
    pub state_update: EngineSyncStateUpdate,
    /// Whether to send an FCU when the requested state already matches the current state.
    force: bool,
}

impl<EngineClient_: EngineClient> SynchronizeTask<EngineClient_> {
    /// Creates a synchronization task that skips redundant forkchoice updates.
    pub const fn new(
        client: Arc<EngineClient_>,
        rollup: Arc<RollupConfig>,
        state_update: EngineSyncStateUpdate,
    ) -> Self {
        Self { client, rollup, state_update, force: false }
    }

    /// Creates a synchronization task that always sends a forkchoice update.
    pub const fn new_forced(
        client: Arc<EngineClient_>,
        rollup: Arc<RollupConfig>,
        state_update: EngineSyncStateUpdate,
    ) -> Self {
        Self { client, rollup, state_update, force: true }
    }

    /// Computes the sync-state update to apply when the EL responds with `Syncing`.
    ///
    /// Until the EL has finished syncing, the state is left untouched. Once EL sync has
    /// completed, already-consolidated safe/local-safe/finalized progress is preserved,
    /// but only for heads at or behind the current unsafe head. `unsafe_head` is never
    /// advanced beyond what the EL can serve.
    fn safe_only_sync_update(&self, state: &EngineState) -> EngineSyncStateUpdate {
        if !state.el_sync_finished {
            return EngineSyncStateUpdate::default();
        }

        let current_unsafe = state.sync_state.unsafe_head();
        let is_not_ahead_of_unsafe = |head: &L2BlockInfo| {
            head.block_info.number < current_unsafe.block_info.number
                || (head.block_info.number == current_unsafe.block_info.number
                    && head.block_info.hash == current_unsafe.block_info.hash)
        };
        EngineSyncStateUpdate {
            // Never advance the unsafe head on a `Syncing` response.
            unsafe_head: None,
            local_safe_head: self.state_update.local_safe_head.filter(is_not_ahead_of_unsafe),
            safe_head: self.state_update.safe_head.filter(is_not_ahead_of_unsafe),
            finalized_head: self.state_update.finalized_head.filter(is_not_ahead_of_unsafe),
        }
    }

    /// Rejects a requested unsafe head below the finalized head before dispatch.
    pub fn validate_update(&self, state: &EngineState) -> Result<(), SynchronizeTaskError> {
        let heads = state.sync_state.updated(self.state_update);
        if heads.unsafe_head().block_info.number < heads.finalized_head().block_info.number {
            return Err(SynchronizeTaskError::FinalizedAheadOfUnsafe(
                heads.unsafe_head().block_info.number,
                heads.finalized_head().block_info.number,
            ));
        }
        Ok(())
    }

    /// Applies only the acknowledged portion of an execution head update.
    pub fn apply_response(
        &self,
        state: &mut EngineState,
        outcome: base_common_types_payload::HeadUpdateOutcome,
    ) -> bool {
        let confirmed = outcome.is_applied();
        if confirmed && !state.el_sync_finished {
            info!(target: "engine", "Finished execution layer sync");
            state.el_sync_finished = true;
        }
        let update = if confirmed { self.state_update } else { self.safe_only_sync_update(state) };
        state.sync_state = state.sync_state.apply_update(update);
        confirmed
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for SynchronizeTask<EngineClient_> {
    type Output = ();
    type Error = SynchronizeTaskError;

    async fn execute(&self, state: &mut EngineState) -> Result<Self::Output, SynchronizeTaskError> {
        // Apply the sync state update to the engine state.
        let new_sync_state = state.sync_state.updated(self.state_update);

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
        if !self.force
            && state.sync_state != Default::default()
            && state.sync_state == new_sync_state
        {
            debug!(target: "engine", ?new_sync_state, "No forkchoice update needed");
            return Ok(());
        }

        self.validate_update(state)?;

        let fcu_time_start = Instant::now();

        // Send the forkchoice update through the input.
        let forkchoice = new_sync_state.create_forkchoice_state();

        // Handle the forkchoice update result.
        // NOTE: it doesn't matter which version we use here, because we're not sending any
        // payload attributes. The forkchoice updated call is version agnostic if no payload
        // attributes are provided.
        let response = self.client.update_heads(forkchoice).await;

        let valid_response = response.map_err(SynchronizeTaskError::from)?;

        let confirmed = self.apply_response(state, valid_response);

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
