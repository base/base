//! The [`Engine`] is a task queue that receives and executes [`EngineTask`]s.

use std::{collections::BinaryHeap, sync::Arc};

use base_consensus_genesis::{RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L2BlockInfo, OpBlockConversionError, to_system_config};
use thiserror::Error;
use tokio::sync::watch::Sender;

use super::EngineTaskExt;
use crate::{
    EngineClient, EngineState, EngineSyncStateUpdate, EngineTask, EngineTaskError,
    EngineTaskErrorSeverity, Metrics, SyncStartError, SynchronizeTask, SynchronizeTaskError,
    find_starting_forkchoice, task_queue::EngineTaskErrors,
};

/// The [`Engine`] task queue.
///
/// Tasks of a shared [`EngineTask`] variant are processed in FIFO order, providing synchronization
/// guarantees for the L2 execution layer and other actors. A priority queue, ordered by
/// [`EngineTask`]'s [`Ord`] implementation, is used to prioritize tasks executed by the
/// [`Engine::drain`] method.
///
///  Because tasks are executed one at a time, they are considered to be atomic operations over the
/// [`EngineState`], and are given exclusive access to the engine state during execution.
///
/// Tasks within the queue are also considered fallible. If they fail with a temporary error,
/// they are not popped from the queue, the error is returned, and they are retried on the
/// next call to [`Engine::drain`].
#[derive(Debug)]
pub struct Engine<EngineClient_: EngineClient> {
    /// The state of the engine.
    state: EngineState,
    /// A sender that can be used to notify the engine actor of state changes.
    state_sender: Sender<EngineState>,
    /// A sender that can be used to notify the engine actor of task queue length changes.
    task_queue_length: Sender<usize>,
    /// The task queue.
    tasks: BinaryHeap<EngineTask<EngineClient_>>,
}

impl<EngineClient_: EngineClient> Engine<EngineClient_> {
    /// Creates a new [`Engine`] with an empty task queue and the passed initial [`EngineState`].
    pub fn new(
        initial_state: EngineState,
        state_sender: Sender<EngineState>,
        task_queue_length: Sender<usize>,
    ) -> Self {
        Self { state: initial_state, state_sender, task_queue_length, tasks: BinaryHeap::default() }
    }

    /// Returns a reference to the inner [`EngineState`].
    pub const fn state(&self) -> &EngineState {
        &self.state
    }

    /// Returns a receiver that can be used to listen to engine state updates.
    pub fn state_subscribe(&self) -> tokio::sync::watch::Receiver<EngineState> {
        self.state_sender.subscribe()
    }

    /// Returns a receiver that can be used to listen to engine queue length updates.
    pub fn queue_length_subscribe(&self) -> tokio::sync::watch::Receiver<usize> {
        self.task_queue_length.subscribe()
    }

    /// Enqueues a new [`EngineTask`] for execution.
    /// Updates the queue length and notifies listeners of the change.
    pub fn enqueue(&mut self, task: EngineTask<EngineClient_>) {
        self.tasks.push(task);
        self.task_queue_length.send_replace(self.tasks.len());
    }

    /// Resets the engine by finding a plausible sync starting point via
    /// [`find_starting_forkchoice`]. The state will be updated to the starting point, and a
    /// forkchoice update will be enqueued in order to reorg the execution layer.
    pub async fn reset(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
    ) -> Result<(L2BlockInfo, BlockInfo, SystemConfig), EngineResetError> {
        // Clear any outstanding tasks to prepare for the reset.
        self.clear();

        let mut start = find_starting_forkchoice(&config, client.as_ref()).await?;

        // Retry to synchronize the engine until we succeeds or a critical error occurs.
        while let Err(err) = SynchronizeTask::new(
            Arc::clone(&client),
            Arc::clone(&config),
            EngineSyncStateUpdate {
                unsafe_head: Some(start.un_safe),
                safe_head: Some(start.safe),
                finalized_head: Some(start.finalized),
            },
        )
        .execute(&mut self.state)
        .await
        {
            match err.severity() {
                EngineTaskErrorSeverity::Temporary
                | EngineTaskErrorSeverity::Flush
                | EngineTaskErrorSeverity::Reset => {
                    warn!(target: "engine", ?err, "Forkchoice update failed during reset. Trying again...");
                    start = find_starting_forkchoice(&config, client.as_ref()).await?;
                }
                EngineTaskErrorSeverity::Critical => {
                    return Err(EngineResetError::Forkchoice(err));
                }
            }
        }

        // Broadcast the updated state so watch-channel subscribers (e.g. op_syncStatus RPC)
        // see the new forkchoice immediately, without waiting for a task to pass through drain().
        self.state_sender.send_replace(self.state);

        // Find the new safe head's L1 origin and SystemConfig.
        let origin_block = start
            .safe
            .l1_origin
            .number
            .saturating_sub(config.channel_timeout(start.safe.block_info.timestamp));
        let l1_origin_info: BlockInfo = client
            .get_l1_block(origin_block.into())
            .await
            .map_err(SyncStartError::RpcError)?
            .ok_or(SyncStartError::BlockNotFound(origin_block.into()))?
            .into_consensus()
            .into();
        let l2_safe_block = client
            .get_l2_block(start.safe.block_info.hash.into())
            .full()
            .await
            .map_err(SyncStartError::RpcError)?
            .ok_or(SyncStartError::BlockNotFound(origin_block.into()))?
            .into_consensus()
            .map_transactions(|t| t.inner.inner.into_inner());
        let system_config = to_system_config(&l2_safe_block, &config)?;

        Metrics::engine_reset_count().increment(1);

        Ok((start.safe, l1_origin_info, system_config))
    }

    /// Seeds the engine sync state from an external source without sending a forkchoice update.
    ///
    /// Pre-populates the [`EngineState`] watch channel so that callers such as `op_syncStatus`
    /// never observe zeros during the bootstrap window. `el_sync_finished` is left unchanged —
    /// the engine has not confirmed validity via FCU and the existing reset-deferral logic must
    /// continue to gate on it.
    pub fn seed_state(&mut self, update: EngineSyncStateUpdate) {
        self.state.sync_state = self.state.sync_state.apply_update(update);
        self.state_sender.send_replace(self.state);
    }

    /// Probes the EL with a bare FCU to determine whether a snap-sync is in progress.
    ///
    /// Unlike [`Engine::reset`], this does not search for a sync starting point —
    /// it FCUs to the state the caller already knows reth holds. Used during bootstrap
    /// when reth is beyond genesis to distinguish two cases:
    ///
    /// - `Ok(true)` — reth responded `Valid`: the canonical chain is complete.
    ///   `el_sync_finished` is set to `true` and `sync_state` is advanced to `update`.
    ///   Subscribers to the state watch channel are notified.
    /// - `Ok(false)` — reth responded `Syncing`: snap-sync is still in progress.
    ///   Both `el_sync_finished` and `sync_state` are left unchanged.
    /// - `Err(_)` — transport or protocol error; the caller should treat this the same
    ///   as `Syncing` (pessimistic fallback).
    ///
    /// **Precondition**: call this while `state.sync_state == Default::default()`.
    /// If [`Engine::seed_state`] has already been called with the same `update`,
    /// [`SynchronizeTask`] will detect an identical state and skip the FCU silently,
    /// leaving `el_sync_finished = false`. Always probe before seeding.
    pub async fn probe_el_sync(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        update: EngineSyncStateUpdate,
    ) -> Result<bool, SynchronizeTaskError> {
        SynchronizeTask::new(client, config, update).execute(&mut self.state).await?;
        self.state_sender.send_replace(self.state);
        Ok(self.state.el_sync_finished)
    }

    /// Clears the task queue.
    pub fn clear(&mut self) {
        self.tasks.clear();
    }

    /// Attempts to drain the queue by executing all [`EngineTask`]s in-order. If any task returns
    /// an error along the way, it is not popped from the queue (in case it must be retried) and
    /// the error is returned.
    pub async fn drain(&mut self) -> Result<(), EngineTaskErrors> {
        // Drain tasks in order of priority, halting on errors for a retry to be attempted.
        while let Some(task) = self.tasks.peek() {
            // Execute the task
            task.execute(&mut self.state).await?;

            // Update the state and notify the engine actor.
            self.state_sender.send_replace(self.state);

            // Pop the task from the queue now that it's been executed.
            self.tasks.pop();

            self.task_queue_length.send_replace(self.tasks.len());
        }

        Ok(())
    }
}

/// An error occurred while attempting to reset the [`Engine`].
#[derive(Debug, Error)]
pub enum EngineResetError {
    /// An error that occurred while updating the forkchoice state.
    #[error(transparent)]
    Forkchoice(#[from] SynchronizeTaskError),
    /// An error occurred while traversing the L1 for the sync starting point.
    #[error(transparent)]
    SyncStart(#[from] SyncStartError),
    /// An error occurred while constructing the `SystemConfig` for the new safe head.
    #[error(transparent)]
    SystemConfigConversion(#[from] OpBlockConversionError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum};
    use base_consensus_genesis::RollupConfig;
    use tokio::sync::watch;

    use crate::{
        Engine, EngineState, EngineSyncStateUpdate,
        test_utils::{test_block_info, test_engine_client_builder},
    };

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
    async fn probe_el_sync_valid_sets_el_sync_finished_and_advances_state() {
        let head = test_block_info(100);
        let safe = test_block_info(90);
        let finalized = test_block_info(80);

        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
        );

        let mut engine = Engine::new(EngineState::default(), state_tx, queue_tx);
        let update = EngineSyncStateUpdate {
            unsafe_head: Some(head),
            safe_head: Some(safe),
            finalized_head: Some(finalized),
        };

        let confirmed = engine
            .probe_el_sync(client, Arc::new(RollupConfig::default()), update)
            .await
            .expect("probe_el_sync should not error on Valid");

        assert!(confirmed, "Valid FCU must return true");
        assert!(engine.state().el_sync_finished, "el_sync_finished must be set after Valid");
        assert_eq!(engine.state().sync_state.unsafe_head().block_info.number, 100);
        assert_eq!(engine.state().sync_state.safe_head().block_info.number, 90);
        assert_eq!(engine.state().sync_state.finalized_head().block_info.number, 80);
    }

    #[tokio::test]
    async fn probe_el_sync_syncing_leaves_state_unchanged() {
        let head = test_block_info(100);

        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let client = Arc::new(
            test_engine_client_builder()
                .with_fork_choice_updated_v3_response(syncing_fcu())
                .build(),
        );

        let mut engine = Engine::new(EngineState::default(), state_tx, queue_tx);
        let update = EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() };

        let confirmed = engine
            .probe_el_sync(client, Arc::new(RollupConfig::default()), update)
            .await
            .expect("probe_el_sync should not error on Syncing");

        assert!(!confirmed, "Syncing FCU must return false");
        assert!(!engine.state().el_sync_finished, "el_sync_finished must remain false");
        assert_eq!(
            engine.state().sync_state.unsafe_head().block_info.number,
            0,
            "sync_state must not advance on Syncing"
        );
    }

    /// Documents the "probe before `seed_state`" invariant: if `seed_state` is called first with
    /// the same update, `SynchronizeTask`'s early-exit guard fires and the FCU is never sent,
    /// leaving `el_sync_finished` = false even when the EL would respond Valid.
    #[tokio::test]
    async fn probe_el_sync_after_seed_state_silently_skips_fcu() {
        let head = test_block_info(100);

        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
        );

        let update = EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() };

        let mut engine = Engine::new(EngineState::default(), state_tx, queue_tx);
        engine.seed_state(update); // seed first — the wrong order

        let confirmed = engine
            .probe_el_sync(Arc::clone(&client), Arc::new(RollupConfig::default()), update)
            .await
            .expect("should not error");

        // SynchronizeTask short-circuits because state.sync_state == new_sync_state.
        // el_sync_finished stays false despite Valid being configured.
        assert!(!confirmed, "probe after seed short-circuits — documents the invariant");
        assert!(!engine.state().el_sync_finished);
    }
}
