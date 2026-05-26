//! The [`Engine`] owns execution-layer state and drains queued [`EngineTask`]s.

use std::{cmp::Reverse, collections::BinaryHeap, sync::Arc, time::Instant};

use alloy_rpc_types_engine::{
    ExecutionPayload, INVALID_FORK_CHOICE_STATE_ERROR, PayloadId, PayloadStatusEnum,
};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_protocol::{AttributesWithParent, BaseBlockConversionError, L2BlockInfo};
use thiserror::Error;
use tokio::sync::watch::Sender;

use super::EngineTaskExt;
use crate::{
    BuildTaskError, EngineBuildError, EngineClient, EngineForkchoiceVersion,
    EngineGetPayloadVersion, EngineState, EngineSyncStateUpdate, EngineTask, EngineTaskError,
    EngineTaskErrorSeverity, ForkchoiceCheckpointReader, Metrics, NoopForkchoiceCheckpointReader,
    SealTaskError, SyncStartError, SynchronizeTask, SynchronizeTaskError,
    find_starting_forkchoice_with_checkpoint_reader, task_queue::EngineTaskErrors,
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
/// next call to [`Engine::drain`]. Tasks that fail with a [`EngineTaskErrorSeverity::Flush`]
/// error are popped from the queue before the error is returned, so that the derivation
/// pipeline can be flushed without the offending task being retried in-place.
#[derive(Debug)]
pub struct Engine<EngineClient_: EngineClient> {
    /// The state of the engine.
    state: EngineState,
    /// A sender that can be used to notify the engine actor of state changes.
    state_sender: Sender<EngineState>,
    /// A sender that can be used to notify the engine actor of task queue length changes.
    task_queue_length: Sender<usize>,
    /// The task queue.
    tasks: BinaryHeap<(EngineTask<EngineClient_>, Reverse<u64>)>,
    /// Monotonic sequence number used to preserve FIFO order within equal-priority tasks.
    next_task_sequence: u64,
}

impl<EngineClient_: EngineClient> Engine<EngineClient_> {
    /// Creates a new [`Engine`] with an empty task queue and the passed initial [`EngineState`].
    pub fn new(
        initial_state: EngineState,
        state_sender: Sender<EngineState>,
        task_queue_length: Sender<usize>,
    ) -> Self {
        Self {
            state: initial_state,
            state_sender,
            task_queue_length,
            tasks: BinaryHeap::default(),
            next_task_sequence: 0,
        }
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

    /// Starts a block build directly against the execution layer.
    pub async fn build(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::BUILD_TASK_LABEL));

        match Self::build_with_state(&self.state, client.as_ref(), config.as_ref(), attributes)
            .await
        {
            Ok(payload_id) => {
                Metrics::engine_task_count(Metrics::BUILD_TASK_LABEL).increment(1);
                Ok(payload_id)
            }
            Err(err) => {
                let severity = err.severity();
                Metrics::engine_task_failure(Metrics::BUILD_TASK_LABEL, severity.as_label())
                    .increment(1);

                match severity {
                    EngineTaskErrorSeverity::Temporary => {
                        trace!(target: "engine", error = %err, "Temporary engine error");
                    }
                    EngineTaskErrorSeverity::Critical => {
                        error!(target: "engine", error = %err, "Critical engine error");
                    }
                    EngineTaskErrorSeverity::Reset => {
                        warn!(target: "engine", "Engine requested derivation reset");
                    }
                    EngineTaskErrorSeverity::Flush => {
                        warn!(target: "engine", "Engine requested derivation flush");
                    }
                }

                Err(err)
            }
        }
    }

    /// Starts a block build using the provided engine state.
    pub async fn build_with_state(
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

    /// Fetches a sealed payload from the execution layer without inserting it.
    pub async fn get_payload(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::GET_PAYLOAD_TASK_LABEL));

        let result = Self::get_payload_with_state(
            &self.state,
            client.as_ref(),
            config.as_ref(),
            payload_id,
            &attributes,
        )
        .await;

        match result {
            Ok(envelope) => {
                Metrics::engine_task_count(Metrics::GET_PAYLOAD_TASK_LABEL).increment(1);
                Ok(envelope)
            }
            Err(err) => {
                Metrics::engine_task_failure(
                    Metrics::GET_PAYLOAD_TASK_LABEL,
                    err.severity().as_label(),
                )
                .increment(1);
                Err(err)
            }
        }
    }

    /// Fetches a sealed payload using the provided engine state.
    pub async fn get_payload_with_state(
        state: &EngineState,
        engine: &EngineClient_,
        cfg: &RollupConfig,
        payload_id: PayloadId,
        payload_attrs: &AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, SealTaskError> {
        debug!(
            target: "engine",
            "Starting new get-payload job"
        );

        let unsafe_block_info = state.sync_state.unsafe_head().block_info;
        let parent_block_info = payload_attrs.parent.block_info;

        if unsafe_block_info.hash != parent_block_info.hash
            || unsafe_block_info.number != parent_block_info.number
        {
            error!(
                target: "engine",
                unsafe_block_info = ?unsafe_block_info,
                parent_block_info = ?parent_block_info,
                "GetPayload attributes parent does not match unsafe head, returning rebuild error"
            );
            Metrics::sequencer_unsafe_head_changed_total().increment(1);
            return Err(SealTaskError::UnsafeHeadChangedSinceBuild);
        }

        Self::fetch_payload(cfg, engine, payload_id, payload_attrs).await
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
    pub async fn start_build(
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

    /// Fetches the payload from the execution layer using the payload timestamp for versioning.
    pub async fn fetch_payload(
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
                        other => {
                            return Err(SealTaskError::UnexpectedPayloadVersion(format!(
                                "{other:?}"
                            )));
                        }
                    },
                }
            }
        };

        Ok(payload_envelope)
    }

    /// Enqueues a new [`EngineTask`] for execution.
    /// Updates the queue length and notifies listeners of the change.
    pub fn enqueue(&mut self, task: EngineTask<EngineClient_>) {
        let sequence = self.next_task_sequence;
        self.next_task_sequence =
            self.next_task_sequence.checked_add(1).expect("engine task sequence overflow");
        self.tasks.push((task, Reverse(sequence)));
        self.task_queue_length.send_replace(self.tasks.len());
        Metrics::engine_task_queue_depth().set(self.tasks.len() as f64);
    }

    /// Resets the engine by finding a plausible sync starting point via
    /// [`crate::find_starting_forkchoice`]. The state will be updated to the starting point, and a
    /// forkchoice update will be enqueued in order to reorg the execution layer.
    pub async fn reset(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
    ) -> Result<L2BlockInfo, EngineResetError> {
        self.reset_with_checkpoint_reader(client, config, &NoopForkchoiceCheckpointReader).await
    }

    /// Like [`Self::reset`], but consults `checkpoint_reader` when reth-labeled blocks cannot be
    /// hydrated because their bodies have been pruned.
    pub async fn reset_with_checkpoint_reader<CheckpointReader>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        checkpoint_reader: &CheckpointReader,
    ) -> Result<L2BlockInfo, EngineResetError>
    where
        CheckpointReader: ForkchoiceCheckpointReader + ?Sized,
    {
        // Clear any outstanding tasks to prepare for the reset.
        self.clear();

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
        // see the new forkchoice immediately, without waiting for a task to pass through drain().
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
        self.next_task_sequence = 0;
        self.task_queue_length.send_replace(0);
        Metrics::engine_task_queue_depth().set(0.0);
    }

    /// Attempts to drain the queue by executing all [`EngineTask`]s in-order. If any task returns
    /// an error along the way, it is not popped from the queue (in case it must be retried) and
    /// the error is returned.
    ///
    /// Exception: tasks that fail with [`EngineTaskErrorSeverity::Flush`] are popped from the
    /// queue before the error is returned. The poisoned task must not be retried in-place — the
    /// engine processor will signal the derivation pipeline to flush its buffered batch/channel
    /// state and the next call to [`Engine::drain`] will proceed with the remaining tasks. Any
    /// [`EngineState`] mutations the task made before failing are published to subscribers prior
    /// to popping, mirroring the success path so watch consumers (e.g. the RPC state view) stay
    /// in sync.
    pub async fn drain(&mut self) -> Result<(), EngineTaskErrors> {
        // Drain tasks in order of priority, halting on errors for a retry to be attempted.
        while let Some((task, _)) = self.tasks.peek() {
            // Execute the task.
            let outcome = match task.execute(&mut self.state).await {
                Ok(()) => Ok(()),
                Err(err) if err.severity() == EngineTaskErrorSeverity::Flush => Err(err),
                Err(err) => return Err(err),
            };

            // Update the state and notify the engine actor.
            self.state_sender.send_replace(self.state);

            // Pop the task from the queue now that it's been executed.
            self.tasks.pop();

            self.task_queue_length.send_replace(self.tasks.len());
            Metrics::engine_task_queue_depth().set(self.tasks.len() as f64);

            outcome?;
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
    SystemConfigConversion(#[from] BaseBlockConversionError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::FixedBytes;
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum};
    use base_common_genesis::RollupConfig;
    use tokio::sync::watch;

    use crate::{
        Engine, EngineState, EngineSyncStateUpdate, EngineTask, EngineTaskError,
        EngineTaskErrorSeverity, InsertPayloadSafety, SealTask, SealTaskError,
        test_utils::{
            TestAttributesBuilder, TestEngineStateBuilder, test_block_info,
            test_engine_client_builder,
        },
    };

    fn invalid_fcu() -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Invalid {
                    validation_error: "malformed transaction".into(),
                },
                latest_valid_hash: Some(FixedBytes([2u8; 32])),
            },
            payload_id: None,
        }
    }

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

    fn test_engine() -> Engine<crate::test_utils::MockEngineClient> {
        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        Engine::new(EngineState::default(), state_tx, queue_tx)
    }

    #[test]
    fn equal_priority_seal_tasks_are_fifo() {
        let client = Arc::new(test_engine_client_builder().build());
        let cfg = Arc::new(RollupConfig::default());
        let attributes = TestAttributesBuilder::new().build();
        let mut engine = test_engine();
        let first_payload_id = PayloadId::new([1; 8]);
        let second_payload_id = PayloadId::new([2; 8]);

        engine.enqueue(EngineTask::Seal(Box::new(SealTask::new(
            Arc::clone(&client),
            Arc::clone(&cfg),
            first_payload_id,
            attributes.clone(),
            InsertPayloadSafety::Unsafe,
            None,
        ))));
        engine.enqueue(EngineTask::Seal(Box::new(SealTask::new(
            Arc::clone(&client),
            Arc::clone(&cfg),
            second_payload_id,
            attributes,
            InsertPayloadSafety::Unsafe,
            None,
        ))));

        let (first, _) = engine.tasks.pop().expect("first task should be queued");
        let (second, _) = engine.tasks.pop().expect("second task should be queued");

        match first {
            EngineTask::Seal(task) => {
                assert_eq!(task.payload_id, first_payload_id);
            }
            other => panic!("expected first seal task, got {other:?}"),
        }

        match second {
            EngineTask::Seal(task) => {
                assert_eq!(task.payload_id, second_payload_id);
            }
            other => panic!("expected second seal task, got {other:?}"),
        }
    }

    #[test]
    fn clear_publishes_zero_queue_length() {
        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, queue_rx) = watch::channel(0usize);
        let client = Arc::new(test_engine_client_builder().build());
        let cfg = Arc::new(RollupConfig::default());
        let mut engine = Engine::new(EngineState::default(), state_tx, queue_tx);

        engine.enqueue(EngineTask::Seal(Box::new(SealTask::new(
            client,
            cfg,
            PayloadId::new([1; 8]),
            TestAttributesBuilder::new().build(),
            InsertPayloadSafety::Unsafe,
            None,
        ))));
        assert_eq!(*queue_rx.borrow(), 1);

        engine.clear();

        assert_eq!(*queue_rx.borrow(), 0);
    }

    fn valid_fcu_with_payload(payload_id: PayloadId) -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: Some(FixedBytes([2u8; 32])),
            },
            payload_id: Some(payload_id),
        }
    }

    #[tokio::test]
    async fn build_with_state_returns_payload_id() {
        let payload_id = PayloadId::new([1u8; 8]);
        let parent_block = test_block_info(0);
        let unsafe_block = test_block_info(1);
        let cfg = RollupConfig::default();
        let client = test_engine_client_builder()
            .with_fork_choice_updated_v2_response(valid_fcu_with_payload(payload_id))
            .build();
        let attributes = TestAttributesBuilder::new().with_parent(parent_block).build();
        let state = TestEngineStateBuilder::new()
            .with_unsafe_head(unsafe_block)
            .with_safe_head(parent_block)
            .with_finalized_head(parent_block)
            .build();

        let result = Engine::build_with_state(&state, &client, &cfg, attributes)
            .await
            .expect("build should return payload id");

        assert_eq!(result, payload_id);
    }

    #[tokio::test]
    async fn get_payload_with_state_rejects_parent_mismatch() {
        let attributes = TestAttributesBuilder::new().build();
        let mismatched_unsafe_head = test_block_info(2);
        let state = TestEngineStateBuilder::new().with_unsafe_head(mismatched_unsafe_head).build();
        let client = test_engine_client_builder().build();

        let result = Engine::get_payload_with_state(
            &state,
            &client,
            &RollupConfig::default(),
            PayloadId::default(),
            &attributes,
        )
        .await;

        assert!(matches!(result, Err(SealTaskError::UnsafeHeadChangedSinceBuild)));
    }

    #[tokio::test]
    async fn get_payload_with_state_propagates_fetch_error() {
        let attributes = TestAttributesBuilder::new().build();
        let state = TestEngineStateBuilder::new().with_unsafe_head(attributes.parent).build();
        let client = test_engine_client_builder().build();

        let result = Engine::get_payload_with_state(
            &state,
            &client,
            &RollupConfig::default(),
            PayloadId::default(),
            &attributes,
        )
        .await;

        assert!(matches!(result, Err(SealTaskError::GetPayloadFailed(_))));
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
            local_safe_head: Some(safe),
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

    /// Regression test: an attr-bearing FCU that returns `PayloadStatusEnum::Invalid` must
    /// surface as [`EngineTaskErrorSeverity::Flush`] from the direct build path.
    #[tokio::test]
    async fn direct_build_invalid_payload_returns_flush() {
        let parent_block = test_block_info(0);
        let unsafe_block = test_block_info(1);
        let attributes_timestamp = unsafe_block.block_info.timestamp;

        let mut cfg = RollupConfig::default();
        // Force the FCU-with-attrs codepath through V3 (Ecotone active at attr timestamp).
        cfg.hardforks.ecotone_time = Some(attributes_timestamp);

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
        let (queue_tx, queue_rx) = watch::channel(0usize);
        let mut engine = Engine::new(initial_state, state_tx, queue_tx);

        let err = engine
            .build(Arc::clone(&client), Arc::clone(&cfg), attributes)
            .await
            .expect_err("invalid FCU must fail build");
        assert_eq!(err.severity(), EngineTaskErrorSeverity::Flush);
        assert_eq!(engine.tasks.len(), 0, "direct build must not enqueue poisoned work");
        assert_eq!(*queue_rx.borrow(), 0, "queue length watch must remain unchanged");
    }
}
