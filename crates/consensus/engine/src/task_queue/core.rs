//! The [`Engine`] owns execution-layer state and executes engine operations serially.

use std::{fmt::Display, future::Future, pin::Pin, sync::Arc, time::Instant};

use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayload, ExecutionPayloadInputV2,
    INVALID_FORK_CHOICE_STATE_ERROR, PayloadId, PayloadStatusEnum, PraguePayloadFields,
};
use base_common_consensus::BaseBlock;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
};
use base_protocol::{AttributesWithParent, BaseBlockConversionError, L2BlockInfo};
use thiserror::Error;
use tokio::{sync::watch::Sender, task::yield_now};

use super::build_and_seal;
use crate::{
    BuildTaskError, ConsolidateInput, ConsolidateTaskError, DelegatedForkchoiceTaskError,
    DelegatedForkchoiceUpdate, EngineBuildError, EngineClient, EngineForkchoiceVersion,
    EngineGetPayloadVersion, EngineState, EngineSyncStateUpdate, EngineTaskError,
    EngineTaskErrorSeverity, FinalizeTaskError, InsertPayloadSafety, InsertTaskError,
    InsertTaskResult, Metrics, SealTaskError, SyncStartError, SynchronizeTask,
    SynchronizeTaskError, find_starting_forkchoice,
};

/// The [`Engine`] state owner.
///
/// The engine actor owns one [`Engine`] and calls direct methods for each request, providing
/// synchronization guarantees for the L2 execution layer and other actors.
///
/// Because operations are executed one at a time, they are considered to be atomic operations over
/// the [`EngineState`], and are given exclusive access to the engine state during execution.
///
#[derive(Debug)]
pub struct Engine {
    /// The state of the engine.
    state: EngineState,
    /// A sender that can be used to notify the engine actor of state changes.
    state_sender: Sender<EngineState>,
}

impl Engine {
    /// Creates a new [`Engine`] with the passed initial [`EngineState`].
    pub const fn new(initial_state: EngineState, state_sender: Sender<EngineState>) -> Self {
        Self { state: initial_state, state_sender }
    }

    /// Returns a reference to the inner [`EngineState`].
    pub const fn state(&self) -> &EngineState {
        &self.state
    }

    /// Returns a receiver that can be used to listen to engine state updates.
    pub fn state_subscribe(&self) -> tokio::sync::watch::Receiver<EngineState> {
        self.state_sender.subscribe()
    }

    /// Starts a block build directly against the execution layer.
    pub async fn build<EngineClient_: EngineClient + 'static>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, BuildTaskError> {
        self.retry_with_severity(Metrics::BUILD_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            let attributes = attributes.clone();
            Box::pin(async move {
                Self::build_with_state(state, client.as_ref(), config.as_ref(), attributes).await
            })
        })
        .await
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

    /// Fetches a sealed payload from the execution layer without inserting it.
    pub async fn get_payload<EngineClient_: EngineClient>(
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
    pub async fn get_payload_with_state<EngineClient_: EngineClient>(
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

    /// Retries an engine operation until it succeeds or returns a non-temporary error.
    pub async fn retry_with_severity<Output, Error, Operation>(
        &mut self,
        label: &'static str,
        mut operation: Operation,
    ) -> Result<Output, Error>
    where
        Output: Send,
        Error: Display + EngineTaskError + Send,
        Operation: for<'state> FnMut(
            &'state mut EngineState,
        ) -> Pin<
            Box<dyn Future<Output = Result<Output, Error>> + Send + 'state>,
        >,
    {
        let _task_timer = base_metrics::timed!(Metrics::engine_task_duration(label));

        loop {
            match operation(&mut self.state).await {
                Ok(output) => {
                    self.state_sender.send_replace(self.state);
                    Metrics::engine_task_count(label).increment(1);
                    return Ok(output);
                }
                Err(err) => {
                    let severity = err.severity();
                    Metrics::engine_task_failure(label, severity.as_label()).increment(1);

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

    /// Consolidates the safe head directly against the execution layer.
    pub async fn consolidate<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        input: ConsolidateInput,
    ) -> Result<(), ConsolidateTaskError>
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::CONSOLIDATE_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            let input = input.clone();
            Box::pin(
                async move { Self::consolidate_with_state(state, client, config, input).await },
            )
        })
        .await
    }

    /// Consolidates the safe head using the provided engine state.
    pub async fn consolidate_with_state<EngineClient_: EngineClient>(
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
    pub async fn build_and_seal_safe_payload<EngineClient_: EngineClient>(
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
    pub async fn reconcile_to_safe_head<EngineClient_: EngineClient>(
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
    pub async fn reconcile_unsafe_to_safe<EngineClient_: EngineClient>(
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
    pub async fn consolidate_safe_head<EngineClient_: EngineClient>(
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
                    warn!(target: "engine", error = ?e, "Failed to construct L2BlockInfo, proceeding to safe payload rebuild");
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

    /// Applies delegated safe and finalized labels directly against the execution layer.
    pub async fn delegated_forkchoice<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        update: DelegatedForkchoiceUpdate,
    ) -> Result<(), DelegatedForkchoiceTaskError>
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::DELEGATED_FORKCHOICE_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            Box::pin(async move {
                Self::delegated_forkchoice_with_state(state, client, config, update).await
            })
        })
        .await
    }

    /// Applies delegated safe and finalized labels using the provided engine state.
    pub async fn delegated_forkchoice_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        update: DelegatedForkchoiceUpdate,
    ) -> Result<(), DelegatedForkchoiceTaskError> {
        Self::consolidate_with_state(
            state,
            Arc::clone(&client),
            Arc::clone(&config),
            ConsolidateInput::BlockInfo(update.safe_l2),
        )
        .await?;

        let actual_safe = state.sync_state.safe_head().block_info.number;
        let Some(remote_finalized) = update.finalized_l2_number else { return Ok(()) };

        let finalized_target = remote_finalized.min(actual_safe);
        let current_finalized = state.sync_state.finalized_head().block_info.number;
        if finalized_target <= current_finalized {
            debug!(
                target: "engine",
                actual_safe,
                current_finalized,
                finalized_target,
                "Skipping delegated finalized update"
            );
            return Ok(());
        }

        Self::finalize_with_state(state, client, config, finalized_target).await?;

        Ok(())
    }

    /// Finalizes an L2 block directly against the execution layer.
    pub async fn finalize<EngineClient_>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        block_number: u64,
    ) -> Result<(), FinalizeTaskError>
    where
        EngineClient_: EngineClient + 'static,
    {
        self.retry_with_severity(Metrics::FINALIZE_TASK_LABEL, move |state| {
            let client = Arc::clone(&client);
            let config = Arc::clone(&config);
            Box::pin(
                async move { Self::finalize_with_state(state, client, config, block_number).await },
            )
        })
        .await
    }

    /// Finalizes an L2 block using the provided engine state.
    pub async fn finalize_with_state<EngineClient_: EngineClient>(
        state: &mut EngineState,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
        block_number: u64,
    ) -> Result<(), FinalizeTaskError> {
        let current_finalized = state.sync_state.finalized_head().block_info.number;
        if block_number < current_finalized {
            debug!(
                target: "engine",
                block_number,
                current_finalized,
                "Skipping stale finalized update"
            );
            return Ok(());
        }

        if state.sync_state.safe_head().block_info.number < block_number {
            return Err(FinalizeTaskError::BlockNotSafe);
        }

        let block_fetch_start = Instant::now();
        let block = client
            .get_l2_block(block_number.into())
            .full()
            .await
            .map_err(FinalizeTaskError::TransportError)?
            .ok_or(FinalizeTaskError::BlockNotFound(block_number))?
            .into_consensus();
        let block_info = L2BlockInfo::from_block_and_genesis(
            &block.map_transactions(|tx| tx.inner.inner.into_inner()),
            &client.cfg().genesis,
        )
        .map_err(FinalizeTaskError::FromBlock)?;
        let block_fetch_duration = block_fetch_start.elapsed();

        let fcu_start = Instant::now();
        SynchronizeTask::new(
            client,
            config,
            EngineSyncStateUpdate { finalized_head: Some(block_info), ..Default::default() },
        )
        .execute(state)
        .await?;
        let fcu_duration = fcu_start.elapsed();
        let total_duration = block_fetch_start.elapsed();
        Metrics::engine_finalize_duration_seconds().record(total_duration.as_secs_f64());

        info!(
            target: "engine",
            hash = %block_info.block_info.hash,
            number = block_info.block_info.number,
            ?block_fetch_duration,
            ?fcu_duration,
            "Updated finalized head"
        );

        Ok(())
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

    /// Fetches the payload from the execution layer using the payload timestamp for versioning.
    pub async fn fetch_payload<EngineClient_: EngineClient>(
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

    /// Resets the engine by finding a plausible sync starting point via
    /// [`find_starting_forkchoice`]. The state will be updated to the starting point, and a
    /// forkchoice update will be sent directly in order to reorg the execution layer.
    pub async fn reset<EngineClient_: EngineClient>(
        &mut self,
        client: Arc<EngineClient_>,
        config: Arc<RollupConfig>,
    ) -> Result<L2BlockInfo, EngineResetError> {
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
    use base_protocol::{L1BlockInfoBedrock, L2BlockInfo};
    use tokio::sync::watch;

    use crate::{
        Engine, EngineState, EngineSyncStateUpdate, EngineTaskError, EngineTaskErrorSeverity,
        InsertPayloadSafety, InsertTaskError, SealTaskError,
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

    fn l2_block_info(block_number: u64, hash: B256, parent_hash: B256) -> L2BlockInfo {
        L2BlockInfo {
            block_info: base_protocol::BlockInfo {
                hash,
                number: block_number,
                parent_hash,
                timestamp: block_number,
            },
            l1_origin: Default::default(),
            seq_num: 0,
        }
    }

    fn bedrock_payload(block_number: u64) -> BaseExecutionPayload {
        bedrock_payload_with_parent(block_number, B256::ZERO)
    }

    fn bedrock_payload_with_parent(block_number: u64, parent_hash: B256) -> BaseExecutionPayload {
        BaseExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash,
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
    async fn local_unsafe_payload_returns_first_temporary_insert_error() {
        let (state_tx, _) = watch::channel(EngineState::default());
        let mut engine = Engine::new(EngineState::default(), state_tx);
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
        );
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload(2),
        };

        let err = engine
            .insert_local_unsafe_payload(client, Arc::new(RollupConfig::default()), envelope)
            .await
            .expect_err("local insert should return the first temporary insert error");

        assert!(matches!(err, InsertTaskError::InsertFailed(_)));
        assert_eq!(engine.state().sync_state, EngineState::default().sync_state);
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
    async fn unsafe_payload_insert_skips_already_processed_payload() {
        let unsafe_head = l2_block_info(2, B256::with_last_byte(2), B256::with_last_byte(1));
        let client = Arc::new(test_engine_client_builder().build());
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload_with_parent(2, B256::with_last_byte(1)),
        };
        let mut state = TestEngineStateBuilder::new().with_unsafe_head(unsafe_head).build();

        let inserted_head = Engine::insert_payload_with_state(
            &mut state,
            client,
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
        .expect("already processed unsafe payload should be skipped");

        assert_eq!(inserted_head, unsafe_head);
        assert_eq!(state.sync_state.unsafe_head(), unsafe_head);
    }

    #[tokio::test]
    async fn unsafe_payload_insert_skips_older_payload() {
        let unsafe_head = l2_block_info(10, B256::with_last_byte(10), B256::with_last_byte(9));
        let client = Arc::new(test_engine_client_builder().build());
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload_with_parent(9, B256::with_last_byte(8)),
        };
        let mut state = TestEngineStateBuilder::new().with_unsafe_head(unsafe_head).build();

        let inserted_head = Engine::insert_payload_with_state(
            &mut state,
            client,
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
        .expect("older unsafe payload should be skipped");

        assert_eq!(inserted_head, unsafe_head);
        assert_eq!(state.sync_state.unsafe_head(), unsafe_head);
    }

    #[tokio::test]
    async fn unsafe_payload_insert_skips_next_block_with_wrong_parent() {
        let unsafe_head = l2_block_info(10, B256::with_last_byte(10), B256::with_last_byte(9));
        let client = Arc::new(test_engine_client_builder().build());
        let envelope = BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: bedrock_payload_with_parent(11, B256::with_last_byte(99)),
        };
        let mut state = TestEngineStateBuilder::new().with_unsafe_head(unsafe_head).build();

        let inserted_head = Engine::insert_payload_with_state(
            &mut state,
            client,
            Arc::new(RollupConfig::default()),
            envelope,
            InsertPayloadSafety::Unsafe,
            false,
        )
        .await
        .expect("wrong-parent next unsafe payload should be skipped");

        assert_eq!(inserted_head, unsafe_head);
        assert_eq!(state.sync_state.unsafe_head(), unsafe_head);
    }

    #[tokio::test]
    async fn probe_el_sync_valid_sets_el_sync_finished_and_advances_state() {
        let head = test_block_info(100);
        let safe = test_block_info(90);
        let finalized = test_block_info(80);

        let (state_tx, _) = watch::channel(EngineState::default());
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
        );

        let mut engine = Engine::new(EngineState::default(), state_tx);
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
        let client = Arc::new(
            test_engine_client_builder()
                .with_fork_choice_updated_v3_response(syncing_fcu())
                .build(),
        );

        let mut engine = Engine::new(EngineState::default(), state_tx);
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
        let client = Arc::new(
            test_engine_client_builder().with_fork_choice_updated_v3_response(valid_fcu()).build(),
        );

        let update = EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() };

        let mut engine = Engine::new(EngineState::default(), state_tx);
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
        let mut engine = Engine::new(initial_state, state_tx);

        let err = engine
            .build(Arc::clone(&client), Arc::clone(&cfg), attributes)
            .await
            .expect_err("invalid FCU must fail build");
        assert_eq!(err.severity(), EngineTaskErrorSeverity::Flush);
        assert_eq!(engine.state().sync_state, initial_state.sync_state);
    }
}
