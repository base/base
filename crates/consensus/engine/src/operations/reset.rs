//! Engine reset and startup synchronization operations.

use std::{sync::Arc, time::Duration};

use base_common_genesis::RollupConfig;
use base_protocol::{BaseBlockConversionError, L2BlockInfo};
use thiserror::Error;
use tokio::time::sleep;

use crate::{
    Engine, EngineClient, EngineSyncStateUpdate, EngineTaskError, EngineTaskErrorSeverity,
    ForkchoiceCheckpointReader, Metrics, NoopForkchoiceCheckpointReader, SyncStartError,
    SynchronizeTask, SynchronizeTaskError, find_starting_forkchoice_with_checkpoint_reader,
};

const ENGINE_RESET_RETRY_DELAY: Duration = Duration::from_millis(50);

impl Engine {
    /// Resets the engine by finding a plausible sync starting point via
    /// [`find_starting_forkchoice_with_checkpoint_reader`]. The state will be updated to the
    /// starting point, and a forkchoice update will be sent directly in order to reorg the
    /// execution layer.
    pub async fn reset<EngineClient_: EngineClient>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
    ) -> Result<L2BlockInfo, EngineResetError> {
        self.reset_with_checkpoint_reader(client, config, &NoopForkchoiceCheckpointReader).await
    }

    /// Like [`Self::reset`], but consults `checkpoint_reader` when reth-labeled blocks cannot be
    /// hydrated because their bodies have been pruned.
    pub async fn reset_with_checkpoint_reader<EngineClient_, CheckpointReader>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        checkpoint_reader: &CheckpointReader,
    ) -> Result<L2BlockInfo, EngineResetError>
    where
        EngineClient_: EngineClient,
        CheckpointReader: ForkchoiceCheckpointReader + ?Sized,
    {
        let mut start = find_starting_forkchoice_with_checkpoint_reader(
            &config,
            client.as_ref(),
            checkpoint_reader,
        )
        .await?;

        // Retry to synchronize the engine until we succeeds or a critical error occurs.
        while let Err(err) = SynchronizeTask::new(
            Arc::clone(&client),
            Arc::clone(&config),
            EngineSyncStateUpdate {
                unsafe_head: Some(start.un_safe),
                local_safe_head: Some(start.safe),
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
                    sleep(ENGINE_RESET_RETRY_DELAY).await;
                    start = find_starting_forkchoice_with_checkpoint_reader(
                        &config,
                        client.as_ref(),
                        checkpoint_reader,
                    )
                    .await?;
                }
                EngineTaskErrorSeverity::Critical => {
                    return Err(EngineResetError::Forkchoice(err));
                }
            }
        }

        // Broadcast the updated state so watch-channel subscribers (e.g. sync-status RPC)
        // see the new forkchoice immediately.
        self.state_sender.send_replace(self.state);

        Metrics::engine_reset_count().increment(1);

        Ok(start.safe)
    }

    /// Seeds the engine sync state from an external source without sending a forkchoice update.
    ///
    /// Pre-populates the [`EngineState`] watch channel so that callers such as sync-status RPC
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
    pub async fn probe_el_sync<EngineClient_: EngineClient>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        update: EngineSyncStateUpdate,
    ) -> Result<bool, SynchronizeTaskError> {
        SynchronizeTask::new(client, config, update).execute(&mut self.state).await?;
        self.state_sender.send_replace(self.state);
        Ok(self.state.el_sync_finished)
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
    SystemConfigConversion(#[from] BaseBlockConversionError),
}
