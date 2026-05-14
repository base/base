//! The [`Engine`] owns execution-layer state and drains queued [`EngineTask`]s.

use std::{collections::BinaryHeap, sync::Arc, time::Instant};

use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayload, ExecutionPayloadInputV2, PayloadId, PayloadStatusEnum,
    PraguePayloadFields,
};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
};
use base_protocol::{AttributesWithParent, BaseBlockConversionError, L2BlockInfo};
use thiserror::Error;
use tokio::{sync::watch::Sender, task::yield_now};

use super::{EngineTaskExt, build_and_seal};
use crate::{
    BuildTaskError, ConsolidateInput, ConsolidateTaskError, EngineBuildError, EngineClient,
    EngineForkchoiceVersion, EngineGetPayloadVersion, EngineState, EngineSyncStateUpdate,
    EngineTask, EngineTaskError, EngineTaskErrorSeverity, InsertPayloadSafety, InsertTaskError,
    InsertTaskResult, Metrics, SealTaskError, SyncStartError, SynchronizeTask,
    SynchronizeTaskError, find_starting_forkchoice, task_queue::EngineTaskErrors,
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

    /// Starts a block build directly against the execution layer.
    pub async fn build(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::BUILD_TASK_LABEL));

        loop {
            match Self::build_with_state(
                &self.state,
                client.as_ref(),
                config.as_ref(),
                attributes.clone(),
            )
            .await
            {
                Ok(payload_id) => {
                    self.state_sender.send_replace(self.state);
                    Metrics::engine_task_count(Metrics::BUILD_TASK_LABEL).increment(1);
                    return Ok(payload_id);
                }
                Err(err) => {
                    let severity = err.severity();
                    Metrics::engine_task_failure(Metrics::BUILD_TASK_LABEL, severity.as_label())
                        .increment(1);

                    match severity {
                        EngineTaskErrorSeverity::Temporary => {
                            trace!(target: "engine", error = %err, "Temporary engine error");
                            yield_now().await;
                        }
                        EngineTaskErrorSeverity::Critical => {
                            error!(target: "engine", error = %err, "Critical engine error");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Reset => {
                            warn!(target: "engine", "Engine requested derivation reset");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Flush => {
                            warn!(target: "engine", "Engine requested derivation flush");
                            return Err(err);
                        }
                    }
                }
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

        self.state_sender.send_replace(self.state);
        Metrics::engine_task_count(Metrics::GET_PAYLOAD_TASK_LABEL).increment(1);

        result
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

    /// Inserts an external unsafe payload, retrying temporary failures like queued insert tasks did.
    pub async fn insert_unsafe_payload(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> InsertTaskResult {
        self.insert_payload_with_retry(client, config, envelope, InsertPayloadSafety::Unsafe).await
    }

    /// Inserts a local sequencer unsafe payload once and returns the insertion result.
    pub async fn insert_local_unsafe_payload(
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
    pub async fn insert_payload_with_retry(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        payload_safety: InsertPayloadSafety,
    ) -> InsertTaskResult {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::INSERT_TASK_LABEL));

        loop {
            match Self::insert_payload_with_state(
                &mut self.state,
                Arc::clone(&client),
                Arc::clone(&config),
                envelope.clone(),
                payload_safety,
                false,
            )
            .await
            {
                Ok(inserted_head) => {
                    self.state_sender.send_replace(self.state);
                    Metrics::engine_task_count(Metrics::INSERT_TASK_LABEL).increment(1);
                    return Ok(inserted_head);
                }
                Err(err) => {
                    let severity = err.severity();
                    Metrics::engine_task_failure(Metrics::INSERT_TASK_LABEL, severity.as_label())
                        .increment(1);

                    match severity {
                        EngineTaskErrorSeverity::Temporary => {
                            trace!(target: "engine", error = %err, "Temporary engine error");
                            yield_now().await;
                        }
                        EngineTaskErrorSeverity::Critical => {
                            error!(target: "engine", error = %err, "Critical engine error");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Reset => {
                            warn!(target: "engine", "Engine requested derivation reset");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Flush => {
                            warn!(target: "engine", "Engine requested derivation flush");
                            return Err(err);
                        }
                    }
                }
            }
        }
    }

    /// Inserts a payload into the execution engine using the provided state.
    pub async fn insert_payload_with_state(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        rollup_config: Arc<RollupConfig>,
        envelope: BaseExecutionPayloadEnvelope,
        payload_safety: InsertPayloadSafety,
        require_unsafe_head_advance: bool,
    ) -> InsertTaskResult {
        let time_start = Instant::now();
        let BaseExecutionPayloadEnvelope { parent_beacon_block_root, execution_payload } = envelope;
        let parent_beacon_block_root = parent_beacon_block_root.unwrap_or_default();
        let new_payload_start = Instant::now();
        let (response, block): (_, BaseBlock) = match execution_payload {
            BaseExecutionPayload::V1(payload) => {
                let block = BaseExecutionPayload::V1(payload.clone())
                    .try_into_block()
                    .map_err(InsertTaskError::FromBlockError)?;
                let payload_input =
                    ExecutionPayloadInputV2 { execution_payload: payload, withdrawals: None };
                (client.new_payload_v2(payload_input).await, block)
            }
            BaseExecutionPayload::V2(payload) => {
                let block = BaseExecutionPayload::V2(payload.clone())
                    .try_into_block()
                    .map_err(InsertTaskError::FromBlockError)?;
                let payload_input = ExecutionPayloadInputV2 {
                    execution_payload: payload.payload_inner,
                    withdrawals: Some(payload.withdrawals),
                };
                (client.new_payload_v2(payload_input).await, block)
            }
            BaseExecutionPayload::V3(payload) => {
                let block = BaseExecutionPayload::V3(payload.clone())
                    .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v3(
                        CancunPayloadFields::new(parent_beacon_block_root, vec![]),
                    ))
                    .map_err(InsertTaskError::FromBlockError)?;
                (client.new_payload_v3(payload, parent_beacon_block_root).await, block)
            }
            BaseExecutionPayload::V4(payload) => {
                let block = BaseExecutionPayload::V4(payload.clone())
                    .try_into_block_with_sidecar(&BaseExecutionPayloadSidecar::v4(
                        CancunPayloadFields::new(parent_beacon_block_root, vec![]),
                        PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                    ))
                    .map_err(InsertTaskError::FromBlockError)?;
                (client.new_payload_v4(payload, parent_beacon_block_root).await, block)
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
        let new_payload_duration = new_payload_start.elapsed();

        let advances_safe_head = payload_safety.advances_safe_head();
        let new_block_ref = L2BlockInfo::from_block_and_genesis(&block, &rollup_config.genesis)
            .map_err(InsertTaskError::L2BlockInfoConstruction)?;

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

        let total_duration = time_start.elapsed();

        info!(
            target: "engine",
            hash = %new_block_ref.block_info.hash,
            number = new_block_ref.block_info.number,
            payload_safety = payload_safety.as_label(),
            total_duration = ?total_duration,
            new_payload_duration = ?new_payload_duration,
            "Inserted new payload"
        );

        Ok(new_block_ref)
    }

    /// Checks the response of the `engine_newPayload` call.
    pub const fn check_new_payload_status(status: &PayloadStatusEnum) -> bool {
        matches!(status, PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing)
    }

    /// Consolidates the safe head directly against the execution layer.
    pub async fn consolidate(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        let _task_timer =
            base_metrics::timed!(Metrics::engine_task_duration(Metrics::CONSOLIDATE_TASK_LABEL));

        loop {
            match Self::consolidate_with_state(
                &mut self.state,
                Arc::clone(&client),
                Arc::clone(&config),
                input.clone(),
            )
            .await
            {
                Ok(()) => {
                    self.state_sender.send_replace(self.state);
                    Metrics::engine_task_count(Metrics::CONSOLIDATE_TASK_LABEL).increment(1);
                    return Ok(());
                }
                Err(err) => {
                    let severity = err.severity();
                    Metrics::engine_task_failure(
                        Metrics::CONSOLIDATE_TASK_LABEL,
                        severity.as_label(),
                    )
                    .increment(1);

                    match severity {
                        EngineTaskErrorSeverity::Temporary => {
                            trace!(target: "engine", error = %err, "Temporary engine error");
                            yield_now().await;
                        }
                        EngineTaskErrorSeverity::Critical => {
                            error!(target: "engine", error = %err, "Critical engine error");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Reset => {
                            warn!(target: "engine", "Engine requested derivation reset");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Flush => {
                            warn!(target: "engine", "Engine requested derivation flush");
                            return Err(err);
                        }
                    }
                }
            }
        }
    }

    /// Consolidates the safe head using the provided engine state.
    pub async fn consolidate_with_state(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        // Behavior depends on how the safe head is provided:
        //
        // - `Attributes`: The safe head is advanced through the normal derivation flow, where the
        //   DerivationActor and EngineActor coordinate both safe and unsafe heads. In this case, we
        //   consolidate as long as the unsafe head has not fallen behind.
        //
        // - `BlockInfo`: The safe head is injected externally by the DerivationActor while
        //   delegating derivation, and is not coordinated with the EngineActor's safe/unsafe heads.
        //   If the injected safe head is ahead of the EngineActor's unsafe head, we reconcile the
        //   unsafe chain up to the safe head instead of consolidating.
        let safe_head_number = match &input {
            ConsolidateInput::Attributes { .. } => state.sync_state.safe_head().block_info.number,
            ConsolidateInput::BlockInfo(safe_block_info) => safe_block_info.block_info.number,
        };
        if safe_head_number < state.sync_state.unsafe_head().block_info.number {
            Self::consolidate_safe_head(state, client, config, input).await
        } else {
            Self::reconcile_unsafe_to_safe(state, client, config, &input).await
        }
    }

    /// Rebuilds and seals attributes when consolidation cannot use the current unsafe block.
    pub async fn build_and_seal_safe_payload(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        attributes: &AttributesWithParent,
    ) -> Result<(), ConsolidateTaskError> {
        build_and_seal(state, client, config, attributes.clone(), InsertPayloadSafety::Safe)
            .await?;

        Ok(())
    }

    /// Reconciles the engine unsafe, local safe, and safe heads to an externally supplied safe head.
    pub async fn reconcile_to_safe_head(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        safe_l2: &L2BlockInfo,
    ) -> Result<(), ConsolidateTaskError> {
        warn!(
            target: "engine",
            safe_l2 = %safe_l2,
            "Apply safe head"
        );

        let fcu_start = Instant::now();

        // We intentionally set unsafe_head to safe_l2 to ensure the engine observes a
        // self-consistent head state. This is required to correctly handle reorgs (where unsafe
        // may be ahead on a non-canonical fork) and to trigger EL sync when the local unsafe head
        // lags behind the safe head.
        SynchronizeTask::new(
            client,
            config,
            EngineSyncStateUpdate {
                unsafe_head: Some(*safe_l2),
                local_safe_head: Some(*safe_l2),
                safe_head: Some(*safe_l2),
                ..Default::default()
            },
        )
        .execute(state)
        .await
        .map_err(|e| {
            warn!(target: "engine", error = ?e, "Apply safe head failed");
            e
        })?;

        let fcu_duration = fcu_start.elapsed();

        info!(
            target: "engine",
            hash = %safe_l2.block_info.hash,
            number = safe_l2.block_info.number,
            fcu_duration = ?fcu_duration,
            "Updated safe head via follow safe"
        );

        Ok(())
    }

    /// Reconciles the unsafe chain to the safe input when direct consolidation cannot be used.
    pub async fn reconcile_unsafe_to_safe(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: &ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        match input {
            ConsolidateInput::Attributes(attributes) => {
                Self::build_and_seal_safe_payload(state, client, config, attributes).await
            }
            ConsolidateInput::BlockInfo(safe_l2) => {
                Self::reconcile_to_safe_head(state, client, config, safe_l2).await
            }
        }
    }

    /// Consolidates the safe head by checking the current unsafe block against the input.
    pub async fn consolidate_safe_head(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError> {
        let global_start = Instant::now();

        let block_num = input.l2_block_number();
        let fetch_start = Instant::now();
        let block = match client.l2_block_by_label(block_num.into()).await {
            Ok(Some(block)) => block,
            Ok(None) => {
                warn!(target: "engine", block_num, "Received `None` block");
                return Err(ConsolidateTaskError::MissingUnsafeL2Block(block_num));
            }
            Err(_) => {
                warn!(target: "engine", "Failed to fetch unsafe l2 block for consolidation");
                return Err(ConsolidateTaskError::FailedToFetchUnsafeL2Block);
            }
        };
        let block_fetch_duration = fetch_start.elapsed();
        let block_hash = block.header.hash;

        if input.is_consistent_with_block(&config, &block) {
            trace!(
                target: "engine",
                input = ?input,
                block_hash = %block_hash,
                "Consolidating engine state",
            );
            match L2BlockInfo::from_block_and_genesis(
                &block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
                &config.genesis,
            ) {
                // Only issue a forkchoice update if the attributes are the last in the span
                // batch. This is an optimization to avoid sending a FCU call for every block in
                // the span batch.
                Ok(block_info) if !input.is_attributes_last_in_span() => {
                    let total_duration = global_start.elapsed();

                    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
                        local_safe_head: Some(block_info),
                        safe_head: Some(block_info),
                        ..Default::default()
                    });

                    info!(
                        target: "engine",
                        hash = %block_info.block_info.hash,
                        number = block_info.block_info.number,
                        ?total_duration,
                        ?block_fetch_duration,
                        "Updated safe head via L1 consolidation"
                    );

                    return Ok(());
                }
                Ok(block_info) => {
                    let fcu_start = Instant::now();

                    SynchronizeTask::new(
                        Arc::clone(&client),
                        Arc::clone(&config),
                        EngineSyncStateUpdate {
                            local_safe_head: Some(block_info),
                            safe_head: Some(block_info),
                            ..Default::default()
                        },
                    )
                    .execute(state)
                    .await
                    .map_err(|e| {
                        warn!(target: "engine", error = ?e, "Consolidation failed");
                        e
                    })?;

                    let fcu_duration = fcu_start.elapsed();
                    let total_duration = global_start.elapsed();

                    info!(
                        target: "engine",
                        hash = %block_info.block_info.hash,
                        number = block_info.block_info.number,
                        ?total_duration,
                        ?block_fetch_duration,
                        fcu_duration = ?fcu_duration,
                        "Updated safe head via L1 consolidation"
                    );

                    return Ok(());
                }
                Err(e) => {
                    warn!(target: "engine", error = ?e, "Failed to construct L2BlockInfo, proceeding to build task");
                }
            }
        }

        debug!(
            target: "engine",
            input = ?input,
            block_hash = %block_hash,
            "ConsolidateInput mismatch! Initiating reorg",
        );
        Self::reconcile_unsafe_to_safe(state, client, config, &input).await
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
            BuildTaskError::EngineBuildError(EngineBuildError::AttributesInsertionFailed(e))
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
                        _ => unreachable!("the response should be a V1 or V2 payload"),
                    },
                }
            }
        };

        Ok(payload_envelope)
    }

    /// Enqueues a new [`EngineTask`] for execution.
    /// Updates the queue length and notifies listeners of the change.
    pub fn enqueue(&mut self, task: EngineTask<EngineClient_>) {
        self.tasks.push(task);
        self.task_queue_length.send_replace(self.tasks.len());
        Metrics::engine_task_queue_depth().set(self.tasks.len() as f64);
    }

    /// Resets the engine by finding a plausible sync starting point via
    /// [`find_starting_forkchoice`]. The state will be updated to the starting point, and a
    /// forkchoice update will be enqueued in order to reorg the execution layer.
    pub async fn reset(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
    ) -> Result<L2BlockInfo, EngineResetError> {
        // Clear any outstanding tasks to prepare for the reset.
        self.clear();

        let mut start = find_starting_forkchoice(&config, client.as_ref()).await?;

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

        Metrics::engine_reset_count().increment(1);

        Ok(start.safe)
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
            Metrics::engine_task_queue_depth().set(self.tasks.len() as f64);
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

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bloom, FixedBytes, U256};
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ExecutionPayloadV2, ForkchoiceUpdated, PayloadId, PayloadStatus,
        PayloadStatusEnum,
    };
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_protocol::L1BlockInfoBedrock;
    use tokio::sync::watch;

    use crate::{
        Engine, EngineState, EngineSyncStateUpdate, InsertPayloadSafety, SealTaskError,
        test_utils::{
            TestAttributesBuilder, TestEngineStateBuilder, test_block_info,
            test_engine_client_builder,
        },
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

    fn valid_fcu_with_payload(payload_id: PayloadId) -> ForkchoiceUpdated {
        ForkchoiceUpdated {
            payload_status: PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: Some(FixedBytes([2u8; 32])),
            },
            payload_id: Some(payload_id),
        }
    }

    fn valid_payload_status() -> PayloadStatus {
        PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(B256::ZERO) }
    }

    fn l1_info_deposit_tx() -> Vec<u8> {
        BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoBedrock::default().encode_calldata(),
            ..Default::default()
        })
        .encoded_2718()
    }

    fn bedrock_payload(block_number: u64) -> BaseExecutionPayload {
        BaseExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash: B256::ZERO,
            fee_recipient: Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: 1,
            extra_data: Default::default(),
            base_fee_per_gas: U256::ZERO,
            block_hash: B256::with_last_byte(block_number as u8),
            transactions: vec![l1_info_deposit_tx().into()],
        })
    }

    fn canyon_payload(block_number: u64) -> BaseExecutionPayload {
        BaseExecutionPayload::V2(ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1_704_992_401,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash: B256::with_last_byte(block_number as u8),
                transactions: vec![l1_info_deposit_tx().into()],
            },
            withdrawals: vec![],
        })
    }

    fn test_insert_client() -> Arc<crate::test_utils::MockEngineClient> {
        Arc::new(
            test_engine_client_builder()
                .with_new_payload_v2_response(valid_payload_status())
                .with_fork_choice_updated_v3_response(valid_fcu())
                .build(),
        )
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
    async fn bedrock_payload_uses_new_payload_v2_with_no_withdrawals() {
        let client = test_insert_client();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload(1),
        };
        let mut state = TestEngineStateBuilder::new().build();

        Engine::insert_payload_with_state(
            &mut state,
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
        .expect("bedrock payload should be imported with engine_newPayloadV2");

        let payload_input = client
            .last_new_payload_v2()
            .await
            .expect("new_payload_v2 should record the payload input");
        assert!(
            payload_input.withdrawals.is_none(),
            "bedrock payload must keep withdrawals unset when sent via engine_newPayloadV2"
        );
    }

    #[tokio::test]
    async fn canyon_payload_uses_new_payload_v2_with_withdrawals() {
        let client = test_insert_client();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: canyon_payload(1),
        };
        let mut state = TestEngineStateBuilder::new().build();

        Engine::insert_payload_with_state(
            &mut state,
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
        .expect("canyon payload should be imported with engine_newPayloadV2");

        let payload_input = client
            .last_new_payload_v2()
            .await
            .expect("new_payload_v2 should record the payload input");
        assert_eq!(
            payload_input.withdrawals,
            Some(vec![]),
            "canyon payload must preserve withdrawals when sent via engine_newPayloadV2"
        );
    }

    #[tokio::test]
    async fn unsafe_payload_insert_advances_only_unsafe_head() {
        let client = test_insert_client();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload(2),
        };
        let mut state = TestEngineStateBuilder::new().build();

        Engine::insert_payload_with_state(
            &mut state,
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
        .expect("unsafe payload should be inserted");

        assert_eq!(state.sync_state.unsafe_head().block_info.number, 2);
        assert_eq!(state.sync_state.local_safe_head().block_info.number, 0);
        assert_eq!(state.sync_state.safe_head().block_info.number, 0);
    }

    #[tokio::test]
    async fn safe_payload_insert_advances_safe_heads() {
        let client = test_insert_client();
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload(3),
        };
        let mut state = TestEngineStateBuilder::new().build();

        Engine::insert_payload_with_state(
            &mut state,
            Arc::clone(&client),
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Safe,
            false,
        )
        .await
        .expect("safe payload should be inserted");

        assert_eq!(state.sync_state.unsafe_head().block_info.number, 3);
        assert_eq!(state.sync_state.local_safe_head().block_info.number, 3);
        assert_eq!(state.sync_state.safe_head().block_info.number, 3);
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
}
