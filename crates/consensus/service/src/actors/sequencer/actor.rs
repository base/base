//! The [`SequencerActor`].

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::PayloadReady;
use base_consensus_derive::AttributesBuilder;
use base_consensus_rpc::SequencerAdminAPIError;
use base_protocol::{L2BlockInfo, to_system_config_from_payload};
use tokio::{
    select,
    sync::{broadcast, mpsc, oneshot},
    task::JoinError,
    time::Interval,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, Metrics, NodeActor, NodeOperatingMode, ResetReason, SequencerAdminQuery,
    UnsafePayloadGossipClient,
    actors::{
        SequencerEngineClient,
        engine::{EngineClientError, EngineClientResult},
        sequencer::{
            BuildPipelineState, ReadyHintDropReason, ScheduledTicker, ShadowSequencingState,
            build::{BuildOutcome, PayloadBuilder, UnsealedPayloadHandle},
            conductor::Conductor,
            error::SequencerActorError,
            l1_origin::OriginSelector,
            recovery::RecoveryModeGuard,
            seal::{PayloadSealer, SealStepError, SealStepOutcome, SealTrigger},
            shadow_funding::ShadowFunding,
        },
    },
};

/// Sender stashed by `stop_sequencer` when waiting for an in-flight seal pipeline to drain.
pub type PendingStopSender = oneshot::Sender<Result<B256, SequencerAdminAPIError>>;

/// The [`SequencerActor`] is responsible for building L2 blocks on top of the current unsafe head
/// and scheduling them to be signed and gossipped by the P2P layer, extending the L2 chain with new
/// blocks.
#[derive(Debug)]
pub struct SequencerActor<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
> where
    AttributesBuilder_: AttributesBuilder,
    Conductor_: Conductor,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient,
{
    /// Receiver for admin API requests.
    pub admin_api_rx: mpsc::Receiver<SequencerAdminQuery>,
    /// Drives L1 origin selection, attribute preparation, and block build initiation.
    pub builder: PayloadBuilder<AttributesBuilder_, OriginSelector_, SequencerEngineClient_>,
    /// The cancellation token, shared between all tasks.
    pub cancellation_token: CancellationToken,
    /// The optional conductor RPC client.
    pub conductor: Option<Conductor_>,
    /// The struct used to interact with the engine.
    pub engine_client: Arc<SequencerEngineClient_>,
    /// Whether the sequencer is active.
    pub is_active: bool,
    /// The node's operating mode.
    pub mode: NodeOperatingMode,
    /// Optional account funding injected into the first private block of every shadow cycle.
    pub shadow_funding: Option<ShadowFunding>,
    /// Shared recovery mode flag.
    pub recovery_mode: RecoveryModeGuard,
    /// The rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
    /// Fallback offset for requesting a Denim payload if no matching frozen-payload hint arrives.
    /// See [`SequencerConfig::seal_offset`].
    ///
    /// [`SequencerConfig::seal_offset`]: crate::SequencerConfig::seal_offset
    pub seal_offset: Duration,
    /// A client to asynchronously sign and gossip built payloads to the network actor.
    pub unsafe_payload_gossip_client: UnsafePayloadGossipClient_,
    /// In-flight seal pipeline. [`Some`] while a sealed payload is being committed,
    /// gossiped, and inserted. [`None`] when idle.
    pub sealer: Option<PayloadSealer>,
    /// Stashed response sender for a pending `stop_sequencer` call that is waiting
    /// for the in-flight seal pipeline to complete before responding.
    pub pending_stop: Option<PendingStopSender>,
    /// Optional frozen-payload hints from the embedded EL; the seal ticker remains the fallback.
    pub payload_ready: Option<broadcast::Receiver<PayloadReady>>,
}

impl<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
>
    SequencerActor<
        AttributesBuilder_,
        Conductor_,
        OriginSelector_,
        SequencerEngineClient_,
        UnsafePayloadGossipClient_,
    >
where
    AttributesBuilder_: AttributesBuilder,
    Conductor_: Conductor,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient,
{
    /// Returns whether this actor is running as a shadow sequencer.
    pub const fn is_shadow_sequencer(&self) -> bool {
        self.mode.is_shadow_sequencer()
    }

    /// Fetches the sealed payload envelope from the engine for the given unsealed handle.
    pub(super) async fn seal_payload(
        &self,
        handle: &UnsealedPayloadHandle,
    ) -> Result<PayloadSealer, SequencerActorError> {
        let seal_request_start = Instant::now();

        let envelope = self
            .engine_client
            .get_sealed_payload(handle.payload_id, handle.attributes_with_parent.clone())
            .await?;

        Metrics::sequencer_block_building_seal_task_duration().record(seal_request_start.elapsed());
        Metrics::sequencer_total_transactions_sequenced()
            .increment(handle.attributes_with_parent.count_transactions());

        Ok(match self.mode {
            NodeOperatingMode::ShadowSequencer { .. } => {
                PayloadSealer::new_private(envelope, "shadow")
            }
            NodeOperatingMode::IsolatedSequencer => {
                PayloadSealer::new_private(envelope, "isolated")
            }
            NodeOperatingMode::Validator | NodeOperatingMode::Sequencer => {
                PayloadSealer::new(envelope)
            }
        })
    }

    /// Attempts to seal a pre-built payload, first checking whether it is still fresh.
    ///
    /// If the unsafe head has advanced past the handle's parent since build time (a P2P block
    /// arrived while the build was in-flight), the handle is discarded and `Ok(None)` is
    /// returned so the caller can restart with [`PayloadBuilder::build`].
    ///
    /// On success returns the new [`PayloadSealer`] together with the elapsed seal duration so
    /// the caller can reschedule the ticker accurately. On a non-fatal seal error returns
    /// `Ok(None)`. On a fatal error the cancellation token is triggered and `Err` is returned.
    pub(super) async fn try_seal_handle(
        &self,
        handle: UnsealedPayloadHandle,
    ) -> Result<Option<(PayloadSealer, Duration)>, SequencerActorError> {
        let current_head = self.engine_client.get_unsafe_head().await?;
        let build_parent = handle.attributes_with_parent.parent().block_info;

        if current_head.block_info.number > build_parent.number {
            warn!(
                target: "sequencer",
                parent_num = build_parent.number,
                current_head_num = current_head.block_info.number,
                "Stale build detected: unsafe head advanced past build parent, discarding"
            );
            Metrics::sequencer_stale_build_discarded_total().increment(1);
            return Ok(None);
        }

        if current_head.block_info.number == build_parent.number
            && current_head.block_info.hash != build_parent.hash
        {
            warn!(
                target: "sequencer",
                parent_num = build_parent.number,
                expected_hash = %build_parent.hash,
                actual_hash = %current_head.block_info.hash,
                "Stale build detected: unsafe head reorged at same height, discarding"
            );
            Metrics::sequencer_stale_build_discarded_total().increment(1);
            return Ok(None);
        }

        // Staleness check above is best-effort: if the unsafe head advances between the
        // get_unsafe_head() call and seal_payload() below, the EL's own validation is
        // the final safety gate.
        let seal_start = Instant::now();
        match self.seal_payload(&handle).await {
            Ok(sealer) => Ok(Some((sealer, seal_start.elapsed()))),
            Err(SequencerActorError::EngineError(EngineClientError::SealError(err))) => {
                if err.is_fatal() {
                    error!(target: "sequencer", error = ?err, "Critical seal task error occurred");
                    Metrics::sequencer_seal_errors_total("true").increment(1);
                    self.cancellation_token.cancel();
                    return Err(SequencerActorError::EngineError(EngineClientError::SealError(
                        err,
                    )));
                }
                warn!(target: "sequencer", error = ?err, "Non-fatal seal error, dropping block");
                Metrics::sequencer_seal_errors_total("false").increment(1);
                Ok(None)
            }
            Err(other_err) => {
                error!(target: "sequencer", error = ?other_err, "Unexpected error sealing payload");
                self.cancellation_token.cancel();
                Err(other_err)
            }
        }
    }

    /// Schedules the initial engine reset request and waits for the unsafe head to be updated.
    ///
    /// If EL sync or canonical catch-up is still in progress, the engine will defer the reset and
    /// return [`EngineClientError::ELSyncing`]. In that case we wait one block time and retry. This
    /// avoids aborting reth's in-progress EL sync and activating before canonical catch-up.
    ///
    /// Admin API queries are serviced throughout — both during reset attempts and during the
    /// backoff sleep — so that control can reach the sequencer while EL sync is in progress.
    /// A successful non-shadow admin start already validated the engine's takeover head; do not
    /// follow it with a startup reset that could replace that head.
    async fn schedule_initial_reset(
        &mut self,
        next_payload: &mut Option<UnsealedPayloadHandle>,
    ) -> Result<(), SequencerActorError> {
        loop {
            let engine_client = Arc::clone(&self.engine_client);
            let shadow_cycle_coordinated = self.is_shadow_sequencer();
            let reset = async {
                if shadow_cycle_coordinated {
                    engine_client
                        .reset_engine_forkchoice_coordinated(ResetReason::ShadowCycle)
                        .await
                } else {
                    engine_client.reset_engine_forkchoice(ResetReason::SequencerStartup).await
                }
            };
            // Once queued, the engine reset outlives this future. Keep its response receiver
            // across admin queries so they cannot discard completion and trigger another reset.
            tokio::pin!(reset);
            let result = loop {
                select! {
                    biased;
                    _ = self.cancellation_token.cancelled() => return Ok(()),
                    Some(query) = self.admin_api_rx.recv() => {
                        let was_active = self.is_active;
                        self.handle_admin_query(next_payload, query).await;
                        if !was_active && self.is_active && !shadow_cycle_coordinated {
                            return Ok(());
                        }
                    }
                    result = &mut reset => break result,
                }
            };
            match result {
                Ok(()) => return Ok(()),
                Err(EngineClientError::ELSyncing) => {
                    info!(target: "sequencer", "EL sync or canonical catch-up in progress; deferring initial engine reset");
                }
                Err(err) => {
                    error!(target: "sequencer", error = ?err, "Failed to send reset request to engine");
                    return Err(err.into());
                }
            }
            // Wait one block time before retrying the reset, but service admin queries
            // and honour cancellation throughout the backoff window.
            let sleep = tokio::time::sleep(Duration::from_secs(self.rollup_config.block_time));
            tokio::pin!(sleep);
            loop {
                select! {
                    biased;
                    _ = self.cancellation_token.cancelled() => return Ok(()),
                    Some(query) = self.admin_api_rx.recv() => {
                        let was_active = self.is_active;
                        self.handle_admin_query(next_payload, query).await;
                        if !was_active && self.is_active && !shadow_cycle_coordinated {
                            return Ok(());
                        }
                    }
                    _ = &mut sleep => break,
                }
            }
        }
    }

    /// Discards private work after a coordinated admin reset and starts a fresh canonical cycle.
    /// The engine reset clears and reanchors its reconciliation gate, so any in-flight seal,
    /// reconciliation attempt, or actor-side cycle state from before the reset is stale.
    async fn reset_shadow_cycle_after_admin(
        &mut self,
        shadow: &mut Option<ShadowSequencingState>,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
    ) -> Result<(), SequencerActorError> {
        // A reset discards the in-flight seal outright, so resolve any pending stop response
        // now rather than leaving it stranded until actor teardown.
        if let Some(tx) = self.pending_stop.take() {
            let result = self.resolve_stop_head().await;
            if tx.send(result).is_err() {
                warn!(target: "sequencer", "Failed to send deferred stop_sequencer response");
            }
        }
        if let Some(shadow_state) = shadow.as_mut() {
            shadow_state.abort_reconciliation();
        }
        self.sealer = None;
        pipeline.pending_build_parent = None;
        let canonical_head = self.engine_client.get_unsafe_head().await?;
        *shadow = Some(ShadowSequencingState::new(canonical_head)?);
        if self.is_active {
            let outcome = self.builder.build_on(canonical_head, self.shadow_funding).await?;
            self.apply_eager_build_outcome(outcome, pipeline, build_ticker);
        } else {
            pipeline.next_payload_to_seal = None;
        }
        Ok(())
    }

    /// Validates and records a private insertion acknowledgement against the current shadow
    /// cycle, cancelling the node on cycle corruption. Returns whether the cycle has reached its
    /// private block limit and reconciliation should start.
    fn on_shadow_insertion(
        &self,
        shadow_state: &mut ShadowSequencingState,
        inserted_head: L2BlockInfo,
    ) -> Result<bool, EngineClientError> {
        let sealer = self.sealer.as_ref().expect("inserted payload must have an active sealer");
        shadow_state
            .cycle
            .validate_insertion(sealer, inserted_head)
            .inspect_err(|_| self.cancellation_token.cancel())?;
        shadow_state
            .cycle
            .record_insertion(
                inserted_head,
                self.mode.shadow_blocks_per_cycle().expect("shadow mode checked").get(),
            )
            .inspect_err(|_| self.cancellation_token.cancel())
    }

    /// Wall-clock deadline for sealing `block_number` if it has not already frozen.
    ///
    /// Denim-active blocks fall back to a fixed offset into their 200ms slot, computed as
    /// `T_N − (interval − seal_offset)` so the first Denim block still seals relative to
    /// its own timestamp. The target ignores how long the previous seal took: lateness
    /// shrinks the current build window instead of shifting the schedule.
    ///
    /// Pre-Denim blocks compensate for the previous seal duration, capped at half the
    /// block interval so one slow seal cannot collapse the next build window to zero and
    /// trigger a fat/thin block oscillation.
    pub(super) fn block_seal_target(
        &self,
        block_number: u64,
        last_seal_duration: Duration,
    ) -> SystemTime {
        let target = UNIX_EPOCH
            + Duration::from_millis(self.rollup_config.l2_block_timestamp_millis(block_number));
        if self.rollup_config.is_denim_active(self.rollup_config.l2_block_timestamp(block_number)) {
            let interval =
                Duration::from_millis(RollupConfig::NATIVE_SUBSECOND_BLOCK_INTERVAL_MILLIS);
            return target - interval.saturating_sub(self.seal_offset);
        }
        let block_interval = Duration::from_secs(self.rollup_config.block_time);
        target - last_seal_duration.min(block_interval / 2)
    }

    fn next_block_seal_target(
        &self,
        block_number: u64,
        last_seal_duration: Duration,
    ) -> SystemTime {
        self.block_seal_target(block_number.saturating_add(1), last_seal_duration)
    }

    /// Wall-clock time `handle`'s build was scheduled to start: its parent's timestamp.
    pub fn slot_start(&self, handle: &UnsealedPayloadHandle) -> SystemTime {
        let parent_number = handle.attributes_with_parent.parent().block_info.number;
        UNIX_EPOCH
            + Duration::from_millis(self.rollup_config.l2_block_timestamp_millis(parent_number))
    }

    /// Records how long after its scheduled start `handle`'s build was requested from the engine.
    pub fn record_build_start(&self, handle: &UnsealedPayloadHandle) {
        let offset = handle
            .build_requested_at
            .duration_since(self.slot_start(handle))
            .unwrap_or(Duration::ZERO);
        Metrics::sequencer_build_start_offset_seconds().record(offset);
    }

    /// Handles one admin API query received while the main loop is running, applying any
    /// resulting shadow-cycle reset and active-state bookkeeping.
    async fn handle_admin_query_tick(
        &mut self,
        pipeline: &mut BuildPipelineState,
        shadow: &mut Option<ShadowSequencingState>,
        build_ticker: &mut ScheduledTicker,
        query: SequencerAdminQuery,
    ) -> Result<(), SequencerActorError> {
        let active_before = self.is_active;

        let reset_requested =
            self.handle_admin_query(&mut pipeline.next_payload_to_seal, query).await;

        if reset_requested && self.is_shadow_sequencer() {
            self.reset_shadow_cycle_after_admin(shadow, pipeline, build_ticker).await?;
        }

        if active_before && !self.is_active {
            pipeline.pending_build_parent = None;
        }

        if !active_before && self.is_active {
            // Clear the previous completion timestamp so the first block after a stop->start
            // cycle does not record the entire idle period as sequencer_block_to_block_duration.
            pipeline.clear_for_active_transition();
            build_ticker.reset_l1_origin_retry_budget();
            build_ticker.reset_immediately();
        }

        Ok(())
    }

    /// Handles the outcome of a completed reconciliation attempt.
    async fn handle_reconciliation_result(
        &mut self,
        pipeline: &mut BuildPipelineState,
        shadow: &mut Option<ShadowSequencingState>,
        reconciliation_ticker: &mut Interval,
        build_ticker: &mut ScheduledTicker,
        task_result: Result<EngineClientResult<Option<L2BlockInfo>>, JoinError>,
    ) -> Result<(), SequencerActorError> {
        if let Some(shadow_state) = shadow.as_mut() {
            shadow_state.reconciliation_task = None;
        }
        let result = task_result.map_err(|error| {
            self.cancellation_token.cancel();
            EngineClientError::ResponseError(format!("shadow reconciliation task failed: {error}"))
        })?;
        match result {
            Ok(Some(head)) => {
                shadow
                    .as_mut()
                    .expect("shadow reconciliation requires cycle state")
                    .cycle
                    .reconcile(head)?;
                if self.is_active {
                    let outcome = self.builder.build_on(head, self.shadow_funding).await?;
                    self.apply_eager_build_outcome(outcome, pipeline, build_ticker);
                }
            }
            // The gate is not ready until every canonical P2P payload has arrived. Back off one
            // block interval instead of hot-looping reconciliation RPCs.
            Ok(None) => reconciliation_ticker
                .reset_after(Duration::from_secs(self.rollup_config.block_time)),
            Err(err) => {
                error!(target: "sequencer", error = ?err, "Shadow reconciliation failed");
                self.cancellation_token.cancel();
                return Err(err.into());
            }
        }
        Ok(())
    }

    /// Handles the outcome of one seal pipeline step.
    async fn handle_seal_step_result(
        &mut self,
        pipeline: &mut BuildPipelineState,
        shadow: &mut Option<ShadowSequencingState>,
        reconciliation_ticker: &mut Interval,
        build_ticker: &mut ScheduledTicker,
        result: Result<SealStepOutcome, SealStepError>,
    ) -> Result<(), SequencerActorError> {
        match result {
            Ok(SealStepOutcome::Inserted(inserted_head)) => {
                let reconcile = match shadow.as_mut() {
                    Some(shadow_state) => self.on_shadow_insertion(shadow_state, inserted_head)?,
                    None => false,
                };

                if let Some(sealer) = self
                    .sealer
                    .as_ref()
                    .filter(|sealer| sealer.matches_inserted_head(inserted_head))
                {
                    match to_system_config_from_payload(
                        &sealer.envelope.execution_payload,
                        &self.rollup_config,
                    ) {
                        Ok(config) => self
                            .builder
                            .attributes_builder
                            .seed_parent_system_config(inserted_head, config),
                        Err(error) => warn!(
                            target: "sequencer",
                            error = %error,
                            block_number = inserted_head.block_info.number,
                            block_hash = %inserted_head.block_info.hash,
                            "Failed to cache acknowledged parent system config"
                        ),
                    }
                }

                if let Some(sealer) = self.sealer.take() {
                    Metrics::sequencer_seal_pipeline_duration().record(sealer.started_at.elapsed());
                }

                if reconcile {
                    pipeline.next_payload_to_seal = None;
                    reconciliation_ticker.reset_immediately();
                }

                if let Some(elapsed) = pipeline.record_block_complete() {
                    Metrics::sequencer_block_to_block_duration().record(elapsed);
                }

                // Respond to a pending stop_sequencer request now that the in-flight seal is
                // complete.
                if let Some(tx) = self.pending_stop.take() {
                    let result = self.resolve_stop_head().await;
                    if tx.send(result).is_err() {
                        warn!(target: "sequencer", "Failed to send deferred stop_sequencer response");
                    }
                }

                let awaiting_reconciliation =
                    shadow.as_ref().is_some_and(ShadowSequencingState::is_awaiting_reconciliation);
                if self.is_active && !awaiting_reconciliation {
                    // Queue the acknowledged parent instead of starting its child build here. Its
                    // timestamp is a hard lower bound for the steady-state child build because
                    // variable getPayload durations can make insertion complete early.
                    let parent_millis = self
                        .rollup_config
                        .l2_block_timestamp_millis(inserted_head.block_info.number);
                    pipeline.pending_build_parent = Some(inserted_head);
                    build_ticker.reset_at(UNIX_EPOCH + Duration::from_millis(parent_millis));
                }
            }
            Ok(SealStepOutcome::Pending) => {}
            Err(err) => {
                let step = self.sealer.as_ref().map(|s| s.state.label()).unwrap_or("unknown");
                warn!(target: "sequencer", error = ?err, step, "Seal step failed, will retry");
            }
        }
        Ok(())
    }

    /// Stores a build started outside the normal ticker path, scheduling only outcomes that need
    /// another attempt. A ready payload keeps the ticker target already owned by the caller.
    fn apply_eager_build_outcome(
        &self,
        outcome: BuildOutcome<UnsealedPayloadHandle>,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
    ) {
        match outcome {
            BuildOutcome::Ready(payload) => {
                self.record_build_start(&payload);
                build_ticker.reset_l1_origin_retry_budget();
                pipeline.next_payload_to_seal = Some(payload);
            }
            BuildOutcome::Deferred | BuildOutcome::AwaitingL1Origin => {
                pipeline.next_payload_to_seal = None;
                build_ticker.schedule_l1_origin_retry();
            }
        }
    }

    /// Stores and schedules the result of a ticker-driven build attempt.
    fn schedule_build_outcome(
        &self,
        outcome: BuildOutcome<UnsealedPayloadHandle>,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
    ) {
        match outcome {
            BuildOutcome::Ready(payload) => {
                self.record_build_start(&payload);
                let target =
                    self.block_seal_target(payload.block_number(), pipeline.last_seal_duration);
                build_ticker.reset_l1_origin_retry_budget();
                pipeline.next_payload_to_seal = Some(payload);
                build_ticker.schedule_after_build(Some(target));
            }
            BuildOutcome::Deferred | BuildOutcome::AwaitingL1Origin => {
                pipeline.next_payload_to_seal = None;
                build_ticker.schedule_l1_origin_retry();
            }
        }
    }

    /// Starts building the queued child on its acknowledged parent. Does not start this build
    /// before its inserted parent's timestamp: if it is already past, the ticker is immediately
    /// runnable.
    async fn start_pending_child_build(
        &mut self,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<(), SequencerActorError> {
        let parent = pipeline.pending_build_parent.take().expect("caller checked Some");
        let outcome = self.builder.build_on(parent, shadow_funding).await?;
        self.schedule_build_outcome(outcome, pipeline, build_ticker);
        Ok(())
    }

    /// Advances the in-flight pre-built payload through sealing, or rebuilds on the current
    /// unsafe head if the seal attempt discarded it as stale.
    async fn advance_seal_or_rebuild(
        &mut self,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<(), SequencerActorError> {
        let handle = pipeline.next_payload_to_seal.take().expect("caller checked Some");
        let handle_block_number = handle.block_number();
        let payload_id = handle.payload_id;
        let trigger = if handle.frozen { SealTrigger::ReadyHint } else { SealTrigger::Deadline };
        let request_offset =
            SystemTime::now().duration_since(self.slot_start(&handle)).unwrap_or(Duration::ZERO);
        match self.try_seal_handle(handle).await? {
            Some((new_sealer, dur)) => {
                Metrics::sequencer_seals_total(trigger.as_str()).increment(1);
                Metrics::sequencer_get_payload_offset_seconds(trigger.as_str())
                    .record(request_offset);
                pipeline.sealed_payload_id = Some(payload_id);
                pipeline.last_seal_duration = dur;
                self.sealer = Some(new_sealer);
                let target =
                    self.next_block_seal_target(handle_block_number, pipeline.last_seal_duration);
                // Do not call build() here. The next payload is built after the engine
                // acknowledges insertion of the sealed payload.
                build_ticker.reset_at(target);
            }
            None => {
                // Stale build or non-fatal seal error: rebuild immediately on the current unsafe
                // head.
                let outcome = self.builder.build(shadow_funding).await?;
                self.schedule_build_outcome(outcome, pipeline, build_ticker);
            }
        }
        Ok(())
    }

    /// Builds fresh on the current unsafe head.
    async fn build_fresh(
        &mut self,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<(), SequencerActorError> {
        let outcome = self.builder.build(shadow_funding).await?;
        self.schedule_build_outcome(outcome, pipeline, build_ticker);
        Ok(())
    }

    /// Wakes the seal ticker only for a frozen payload belonging to the active Denim build, and
    /// counts every other hint by the reason it was dropped.
    pub fn handle_payload_ready(
        &self,
        ready: PayloadReady,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
    ) {
        match self.ready_hint_drop_reason(ready, pipeline) {
            Some(reason) => {
                Metrics::sequencer_payload_ready_hints_dropped_total(reason.as_str()).increment(1);
            }
            None => {
                if let Some(handle) = &mut pipeline.next_payload_to_seal {
                    handle.frozen = true;
                }
                build_ticker.reset_immediately();
            }
        }
    }

    /// Returns why `ready` cannot seal the active build, or `None` if it matches that build.
    pub fn ready_hint_drop_reason(
        &self,
        ready: PayloadReady,
        pipeline: &BuildPipelineState,
    ) -> Option<ReadyHintDropReason> {
        if self.sealer.is_some() {
            return Some(ReadyHintDropReason::AlreadySealing);
        }
        if !self.is_active {
            return Some(ReadyHintDropReason::Inactive);
        }
        let active_build = pipeline
            .next_payload_to_seal
            .as_ref()
            .filter(|handle| handle.payload_id == ready.payload_id);
        let Some(handle) = active_build else {
            // Only a hint that matches no active build can have lost the race to the deadline:
            // a rebuild of the last sealed block reuses its payload id and must still seal early.
            return Some(if pipeline.sealed_payload_id == Some(ready.payload_id) {
                ReadyHintDropReason::AlreadySealing
            } else if pipeline.next_payload_to_seal.is_none() {
                ReadyHintDropReason::NoBuild
            } else {
                ReadyHintDropReason::PayloadIdMismatch
            });
        };
        if ready.parent_hash != handle.attributes_with_parent.parent().block_info.hash {
            return Some(ReadyHintDropReason::ParentMismatch);
        }
        if !self
            .rollup_config
            .is_denim_active(self.rollup_config.l2_block_timestamp(handle.block_number()))
        {
            return Some(ReadyHintDropReason::PreDenim);
        }
        None
    }

    /// Dispatches one build-ticker tick to whichever of the three build states is active:
    /// a queued child build gated on its parent's timestamp, an in-flight payload ready to seal,
    /// or neither, in which case a fresh build is started on the current unsafe head.
    async fn handle_build_tick(
        &mut self,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
        shadow_funding: Option<ShadowFunding>,
    ) -> Result<(), SequencerActorError> {
        if pipeline.pending_build_parent.is_some() {
            self.start_pending_child_build(pipeline, build_ticker, shadow_funding).await
        } else if pipeline.next_payload_to_seal.is_some() {
            self.advance_seal_or_rebuild(pipeline, build_ticker, shadow_funding).await
        } else {
            self.build_fresh(pipeline, build_ticker, shadow_funding).await
        }
    }
}

#[async_trait]
impl<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
> NodeActor
    for SequencerActor<
        AttributesBuilder_,
        Conductor_,
        OriginSelector_,
        SequencerEngineClient_,
        UnsafePayloadGossipClient_,
    >
where
    AttributesBuilder_: AttributesBuilder + Sync + 'static,
    Conductor_: Conductor + Sync + 'static,
    OriginSelector_: OriginSelector + Sync + 'static,
    SequencerEngineClient_: SequencerEngineClient + Sync + 'static,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient + Sync + 'static,
{
    type Error = SequencerActorError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        let mut build_ticker =
            ScheduledTicker::new(Duration::from_secs(self.rollup_config.block_time));

        self.update_metrics();

        let mut pipeline = BuildPipelineState::default();

        // Reset the engine state prior to beginning block building.
        // Admin API queries are serviced during this phase (see schedule_initial_reset).
        self.schedule_initial_reset(&mut pipeline.next_payload_to_seal).await?;
        let mut shadow: Option<ShadowSequencingState> = if self.is_shadow_sequencer() {
            Some(ShadowSequencingState::new(self.engine_client.get_unsafe_head().await?)?)
        } else {
            None
        };
        // Reconciliation readiness is polled at block cadence. Skipping missed ticks avoids a
        // retry burst after the actor is delayed by sealing or engine work.
        let mut reconciliation_ticker =
            tokio::time::interval(Duration::from_secs(self.rollup_config.block_time));
        reconciliation_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => {
                    if let Some(shadow_state) = shadow.as_mut() {
                        shadow_state.abort_reconciliation();
                    }
                    info!(target: "sequencer", "Received shutdown signal. Exiting sequencer task.");
                    return Ok(());
                }
                Some(query) = self.admin_api_rx.recv() => {
                    self.handle_admin_query_tick(&mut pipeline, &mut shadow, &mut build_ticker, query).await?;
                }
                _ = reconciliation_ticker.tick(), if shadow.as_ref().is_some_and(|s| s.is_awaiting_reconciliation() && s.reconciliation_task.is_none()) && self.sealer.is_none() => {
                    let shadow_state = shadow.as_mut().expect("reconciliation target checked");
                    shadow_state.reconciliation_task =
                        Some(shadow_state.cycle.start_reconciliation(Arc::clone(&self.engine_client))?);
                }
                task_result = async {
                    match shadow.as_mut().and_then(|s| s.reconciliation_task.as_mut()) {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_reconciliation_result(
                        &mut pipeline,
                        &mut shadow,
                        &mut reconciliation_ticker,
                        &mut build_ticker,
                        task_result,
                    ).await?;
                }
                // Drive the seal pipeline (commit → gossip → insert) one step per iteration.
                // The ticker arm is gated on `sealer.is_none()` so the two are mutually
                // exclusive — when a seal is in-flight the ticker cannot fire and interrupt it.
                Some(result) = async {
                    match self.sealer.as_mut() {
                        Some(s) => Some(s.step(
                            &self.conductor,
                            &self.unsafe_payload_gossip_client,
                            &self.engine_client,
                        ).await),
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_seal_step_result(
                        &mut pipeline,
                        &mut shadow,
                        &mut reconciliation_ticker,
                        &mut build_ticker,
                        result,
                    ).await?;
                }
                ready = async {
                    match self.payload_ready.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending().await,
                    }
                }, if self.sealer.is_none() => {
                    match ready {
                        Ok(ready) => self.handle_payload_ready(ready, &mut pipeline, &mut build_ticker),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            Metrics::sequencer_payload_ready_hints_dropped_total(
                                ReadyHintDropReason::Lagged.as_str(),
                            )
                            .increment(skipped);
                            debug!(target: "sequencer", skipped, "Lagged frozen-payload hints; seal deadline remains active");
                        }
                        Err(broadcast::error::RecvError::Closed) => self.payload_ready = None,
                    }
                }
                // Tick is gated on `self.sealer.is_none()` to make the ticker and sealer arms
                // mutually exclusive. In catch-up mode reset_immediately() fires every tick,
                // making the ticker Poll::Ready at the same time as the sealer's step().await
                // is Poll::Pending. Disabling the ticker while a seal is in-flight lets the
                // sealer arm complete all three steps (commit → gossip → insert) before the
                // next block starts, so the canonical head actually advances.
                _ = build_ticker.tick(), if self.is_active && self.sealer.is_none() && shadow.as_ref().is_none_or(|s| !s.is_awaiting_reconciliation()) => {
                    let shadow_funding = shadow
                        .as_ref()
                        .filter(|state| state.cycle.is_at_start())
                        .and(self.shadow_funding);
                    self.handle_build_tick(&mut pipeline, &mut build_ticker, shadow_funding).await?;
                }
            }
        }
    }
}

impl<
    AttributesBuilder_,
    Conductor_,
    OriginSelector_,
    SequencerEngineClient_,
    UnsafePayloadGossipClient_,
> CancellableContext
    for SequencerActor<
        AttributesBuilder_,
        Conductor_,
        OriginSelector_,
        SequencerEngineClient_,
        UnsafePayloadGossipClient_,
    >
where
    AttributesBuilder_: AttributesBuilder,
    Conductor_: Conductor,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    UnsafePayloadGossipClient_: UnsafePayloadGossipClient,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{B256, Sealed};
    use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus, PayloadStatusEnum};
    use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
    use base_common_genesis::{
        BaseUpgradeConfig, ChainGenesis, RollupConfig, SystemConfig, UpgradeConfig,
    };
    use base_common_rpc_types_engine::{
        BaseExecutionPayload, BaseExecutionPayloadEnvelope, BasePayloadAttributes,
    };
    use base_consensus_derive::test_utils::TestAttributesBuilder;
    use base_consensus_engine::{Engine, test_utils::TestEngineStateBuilder};
    use base_protocol::{AttributesWithParent, BlockInfo, L1BlockInfoBedrock};
    use futures::FutureExt;
    #[cfg(feature = "metrics")]
    use metrics_util::{
        CompositeKey,
        debugging::{DebugValue, DebuggingRecorder},
    };
    use rstest::rstest;
    use tokio::sync::watch;

    use super::*;
    #[cfg(feature = "metrics")]
    use crate::L1OriginSelectorError;
    use crate::{
        EngineActorRequest, EngineProcessor, EngineRequestReceiver, MockConductor,
        MockEngineDerivationClient, MockOriginSelector, MockSequencerEngineClient,
        MockUnsafePayloadGossipClient, QueuedSequencerEngineClient,
        SequencerEngineRequestCoordinator,
        actors::sequencer::tests::test_actor,
        test_utils::{EngineClientCall, FakeEngineClient, ScriptedForkchoiceResponse},
    };

    #[tokio::test(start_paused = true)]
    async fn startup_reset_survives_admin_queries() {
        let config = Arc::new(RollupConfig { block_time: 2, ..Default::default() });
        let (request_tx, mut request_rx) = mpsc::channel(8);
        let (_head_tx, head_rx) = watch::channel(L2BlockInfo::default());
        let (_state_tx, state_rx) = watch::channel(TestEngineStateBuilder::new().build());
        let engine_client =
            Arc::new(QueuedSequencerEngineClient::new(request_tx, head_rx, state_rx));
        let (admin_tx, admin_rx) = mpsc::channel(8);
        let cancellation = CancellationToken::new();
        let recovery_mode = RecoveryModeGuard::new(false);
        let actor = SequencerActor {
            admin_api_rx: admin_rx,
            builder: PayloadBuilder {
                attributes_builder: TestAttributesBuilder::default(),
                engine_client: Arc::clone(&engine_client),
                origin_selector: MockOriginSelector::new(),
                recovery_mode: recovery_mode.clone(),
                rollup_config: Arc::clone(&config),
            },
            cancellation_token: cancellation.clone(),
            conductor: None::<MockConductor>,
            engine_client,
            is_active: false,
            mode: NodeOperatingMode::Sequencer,
            shadow_funding: None,
            recovery_mode,
            rollup_config: config,
            seal_offset: base_protocol::DEFAULT_SEAL_OFFSET,
            unsafe_payload_gossip_client: MockUnsafePayloadGossipClient::new(),
            sealer: None,
            pending_stop: None,
            payload_ready: None,
        };
        let actor = tokio::spawn(actor.start(()));
        let EngineActorRequest::ResetRequest(reset) =
            tokio::time::timeout(Duration::from_secs(1), request_rx.recv()).await.unwrap().unwrap()
        else {
            panic!("expected the startup reset");
        };
        assert_eq!(reset.reason, ResetReason::SequencerStartup);

        // Receiving the request first ensures that the reset is in flight, rather than
        // the admin query winning before it is ever submitted. Hold its reply beyond
        // the retry backoff while checking that read-only admin queries stay responsive.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            let (result_tx, result_rx) = oneshot::channel();
            admin_tx.send(SequencerAdminQuery::SequencerActive(result_tx)).await.unwrap();
            assert!(
                !tokio::time::timeout(Duration::from_secs(1), result_rx)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
            );
        }

        let delivered = reset.result_tx.send(Ok(())).await;
        let next_request = tokio::time::timeout(Duration::from_secs(4), request_rx.recv()).await;
        cancellation.cancel();
        actor.await.unwrap().unwrap();

        assert!(delivered.is_ok(), "admin queries must not abandon the in-flight startup reset");
        assert!(next_request.is_err(), "a successful startup reset must not be resubmitted");
    }

    #[rstest]
    #[tokio::test(start_paused = true)]
    async fn conductor_takeover_without_gossip_starts_building(
        #[values(false, true)] shadow: bool,
        #[values(false, true)] during_backoff: bool,
    ) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let head = L2BlockInfo {
            block_info: BlockInfo {
                number: 100,
                hash: B256::with_last_byte(100),
                timestamp: now,
                ..Default::default()
            },
            ..Default::default()
        };
        let config = Arc::new(RollupConfig {
            block_time: 2,
            genesis: ChainGenesis { l2_time: now - 200, ..Default::default() },
            upgrades: UpgradeConfig { ecotone_time: Some(0), ..Default::default() },
            ..Default::default()
        });
        let el = Arc::new(FakeEngineClient::new(Arc::clone(&config)));
        el.set_l2_block_info_by_label(BlockNumberOrTag::Latest, head);
        let el_handle = el.handle();
        el_handle.push_scripted_fcu_v3([None, Some(PayloadId::new([1; 8]))].map(|payload_id| {
            ScriptedForkchoiceResponse::Ok(ForkchoiceUpdated {
                payload_status: PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: Some(head.block_info.hash),
                },
                payload_id,
            })
        }));
        let safe = L2BlockInfo {
            block_info: BlockInfo {
                number: 90,
                hash: B256::with_last_byte(90),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = TestEngineStateBuilder::new()
            .with_unsafe_head(head)
            .with_safe_head(safe)
            .with_finalized_head(safe)
            .build();
        let (state_tx, state_rx) = watch::channel(state);
        let (queue_tx, _) = watch::channel(0usize);
        let mut derivation = MockEngineDerivationClient::new();
        derivation.expect_send_new_engine_safe_head().returning(|_| Ok(()));
        derivation.expect_notify_sync_completed().returning(|_| Ok(()));
        derivation.expect_send_signal().returning(|_| Ok(()));
        let processor = EngineProcessor::new(
            el,
            Arc::clone(&config),
            derivation,
            Engine::new(state, state_tx, queue_tx),
        );
        let (request_tx, request_rx) = mpsc::channel(8);
        let (head_tx, mut head_rx) = watch::channel(L2BlockInfo::default());
        let coordinator = SequencerEngineRequestCoordinator::new(
            processor,
            if shadow {
                NodeOperatingMode::ShadowSequencer { blocks_per_cycle: std::num::NonZeroU64::MIN }
            } else {
                NodeOperatingMode::Sequencer
            },
            None,
            true,
            head_tx,
        )
        .start(request_rx);
        tokio::time::timeout(Duration::from_secs(1), head_rx.wait_for(|value| *value == head))
            .await
            .unwrap()
            .unwrap();
        let engine_client =
            Arc::new(QueuedSequencerEngineClient::new(request_tx, head_rx, state_rx));
        assert!(matches!(
            engine_client.reset_engine_forkchoice(ResetReason::SequencerStartup).await,
            Err(EngineClientError::ELSyncing)
        ));

        let mut conductor = MockConductor::new();
        conductor.expect_leader().once().returning(|| Ok(true));
        let mut origin_selector = MockOriginSelector::new();
        origin_selector.expect_next_l1_origin().returning(|_| Ok(BlockInfo::default()));
        let gossip = MockUnsafePayloadGossipClient::new();
        let mut attributes = BasePayloadAttributes::default();
        attributes.payload_attributes.timestamp = now + 2;
        let recovery_mode = RecoveryModeGuard::new(false);
        let cancellation = CancellationToken::new();
        let (admin_tx, admin_rx) = mpsc::channel(8);
        let actor = SequencerActor {
            admin_api_rx: admin_rx,
            builder: PayloadBuilder {
                attributes_builder: TestAttributesBuilder {
                    attributes: vec![Ok(attributes)],
                    ..Default::default()
                },
                engine_client: Arc::clone(&engine_client),
                origin_selector,
                recovery_mode: recovery_mode.clone(),
                rollup_config: Arc::clone(&config),
            },
            cancellation_token: cancellation.clone(),
            conductor: Some(conductor),
            engine_client: Arc::clone(&engine_client),
            is_active: false,
            mode: if shadow {
                NodeOperatingMode::ShadowSequencer {
                    blocks_per_cycle: std::num::NonZeroU64::new(1).unwrap(),
                }
            } else {
                NodeOperatingMode::Sequencer
            },
            shadow_funding: None,
            recovery_mode,
            rollup_config: config,
            seal_offset: base_protocol::DEFAULT_SEAL_OFFSET,
            unsafe_payload_gossip_client: gossip,
            sealer: None,
            pending_stop: None,
            payload_ready: None,
        };
        let actor = tokio::spawn(actor.start(()));
        if during_backoff {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (result_tx, result_rx) = oneshot::channel();
        admin_tx
            .send(SequencerAdminQuery::StartSequencer(head.block_info.hash, result_tx))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), result_rx).await.unwrap().unwrap().unwrap();

        let built = tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(fcs) = el_handle.calls().into_iter().find_map(|call| match call {
                    EngineClientCall::ForkChoiceUpdatedV3 { fcs, payload_attributes }
                        if payload_attributes.is_some() =>
                    {
                        Some(fcs)
                    }
                    _ => None,
                }) {
                    break fcs;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        cancellation.cancel();
        actor.await.unwrap().unwrap();
        drop(engine_client);
        assert!(matches!(coordinator.await.unwrap(), Err(crate::EngineError::ChannelClosed)));
        if shadow {
            assert!(built.is_err(), "admin start must not bypass shadow catch-up");
        } else {
            assert_eq!(
                built.expect("takeover must build without gossip").head_block_hash,
                head.block_info.hash
            );
        }
    }

    #[cfg(feature = "metrics")]
    type RecordedMetric =
        (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

    /// Returns what was recorded for the `base_node` metric `name` carrying `label`, if anything.
    #[cfg(feature = "metrics")]
    fn recorded<'a>(
        metrics: &'a [RecordedMetric],
        name: &str,
        label: (&str, &str),
    ) -> Option<&'a DebugValue> {
        metrics
            .iter()
            .find(|(key, ..)| {
                key.key().name() == format!("base_node.{name}")
                    && key.key().labels().any(|l| (l.key(), l.value()) == label)
            })
            .map(|(.., value)| value)
    }

    /// Returns the samples of a histogram recorded exactly once per label.
    #[cfg(feature = "metrics")]
    fn recorded_samples(metrics: &[RecordedMetric], name: &str, label: (&str, &str)) -> Vec<f64> {
        match recorded(metrics, name, label) {
            Some(DebugValue::Histogram(samples)) => {
                samples.iter().map(|sample| sample.into_inner()).collect()
            }
            other => panic!("expected a {name} histogram for {label:?}, got {other:?}"),
        }
    }

    #[rstest]
    #[case("ready", true)]
    #[case("lagged", true)]
    #[case("closed", false)]
    #[case("missing", false)]
    #[tokio::test(start_paused = true)]
    async fn actor_receives_ready_hints_or_uses_deadline(
        #[case] delivery: &'static str,
        #[case] early: bool,
    ) {
        #[cfg(feature = "metrics")]
        let recorder = DebuggingRecorder::new();
        #[cfg(feature = "metrics")]
        let snapshotter = recorder.snapshotter();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut actor = test_actor();
        let future = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 10;
        let config = Arc::new(RollupConfig {
            genesis: ChainGenesis { l2_time: future, ..Default::default() },
            block_time: 2,
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(0), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        });
        actor.rollup_config = Arc::clone(&config);
        actor.builder.rollup_config = config;
        let mut attributes = BasePayloadAttributes::default();
        attributes.payload_attributes.timestamp = future;
        actor.builder.attributes_builder.attributes = vec![Ok(attributes)];
        actor
            .builder
            .origin_selector
            .expect_next_l1_origin()
            .times(1)
            .return_once(|_| Ok(BlockInfo::default()));
        let (tx, rx) = broadcast::channel(1);
        actor.payload_ready = Some(rx);
        let keepalive = (delivery == "missing").then(|| tx.clone());
        let (built_tx, built_rx) = oneshot::channel();
        let (sealed_tx, mut sealed_rx) = oneshot::channel();
        let cancel = actor.cancellation_token.clone();
        let mut engine = MockSequencerEngineClient::new();
        engine.expect_reset_engine_forkchoice().times(1).return_once(|_| Ok(()));
        engine.expect_get_unsafe_head().times(2).returning(|| Ok(L2BlockInfo::default()));
        engine.expect_start_build_block().times(1).return_once(move |attributes| {
            let id = PayloadId::new([7; 8]);
            let ready =
                PayloadReady { payload_id: id, parent_hash: attributes.parent().block_info.hash };
            if delivery == "ready" || delivery == "lagged" {
                tx.send(ready).unwrap();
                if delivery == "lagged" {
                    tx.send(ready).unwrap();
                }
            }
            // Notify before FCU returns: the actor must retain this fast-build signal.
            built_tx.send(()).unwrap();
            Ok(id)
        });
        engine.expect_get_sealed_payload().times(1).return_once(move |id, _| {
            assert_eq!(id, PayloadId::new([7; 8]));
            sealed_tx.send(()).unwrap();
            cancel.cancel();
            Ok(valid_sealer().0.envelope)
        });
        let engine = Arc::new(engine);
        actor.engine_client = Arc::clone(&engine);
        actor.builder.engine_client = engine;
        let task = tokio::spawn(actor.start(()));
        built_rx.await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), &mut sealed_rx).await;
        if early {
            first.expect("ready hint must fetch before the seal deadline").unwrap();
        } else {
            assert!(first.is_err(), "no hint must preserve the scheduled seal");
            tokio::time::timeout(Duration::from_secs(20), sealed_rx).await.unwrap().unwrap();
        }
        task.await.unwrap().unwrap();
        drop(keepalive);

        #[cfg(feature = "metrics")]
        {
            let metrics = snapshotter.snapshot().into_vec();
            let (trigger, other) =
                if early { ("ready_hint", "deadline") } else { ("deadline", "ready_hint") };
            assert_eq!(
                recorded(&metrics, "sequencer_seals_total", ("trigger", trigger)),
                Some(&DebugValue::Counter(1))
            );
            assert_eq!(recorded(&metrics, "sequencer_seals_total", ("trigger", other)), None);
            // Two hints into a one-slot channel overwrite the first before the actor reads it.
            let lagged = (delivery == "lagged").then_some(DebugValue::Counter(1));
            assert_eq!(
                recorded(
                    &metrics,
                    "sequencer_payload_ready_hints_dropped_total",
                    ("reason", "lagged")
                ),
                lagged.as_ref()
            );
        }
    }

    /// Seals block 1, whose slot began when its parent's timestamp passed about five seconds
    /// ago.
    #[cfg(feature = "metrics")]
    #[rstest]
    #[case::ready_hint(true, "ready_hint", "deadline")]
    #[case::deadline(false, "deadline", "ready_hint")]
    #[tokio::test(start_paused = true)]
    async fn seal_records_its_trigger_and_get_payload_offset_from_slot_start(
        #[case] hinted: bool,
        #[case] trigger: &str,
        #[case] other: &str,
    ) {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut actor = test_actor();
        let slot_start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 5;
        let config = Arc::make_mut(&mut actor.rollup_config);
        config.block_time = 2;
        config.genesis.l2_time = slot_start;
        config.upgrades.base.denim = Some(0);
        let (ready, mut pipeline) = ready_build();
        let parent =
            *pipeline.next_payload_to_seal.as_ref().unwrap().attributes_with_parent.parent();
        let (sealer, _, _) = valid_sealer();
        let mut engine = MockSequencerEngineClient::new();
        engine.expect_get_unsafe_head().times(1).return_once(move || Ok(parent));
        engine.expect_get_sealed_payload().times(1).return_once(move |_, _| Ok(sealer.envelope));
        actor.engine_client = Arc::new(engine);
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        let since_slot_start =
            || (UNIX_EPOCH + Duration::from_secs(slot_start)).elapsed().unwrap().as_secs_f64();

        let before = since_slot_start();
        if hinted {
            actor.handle_payload_ready(ready, &mut pipeline, &mut ticker);
        }
        actor.handle_build_tick(&mut pipeline, &mut ticker, None).await.unwrap();
        let after = since_slot_start();

        let metrics = snapshotter.snapshot().into_vec();
        assert_eq!(
            recorded(&metrics, "sequencer_seals_total", ("trigger", trigger)),
            Some(&DebugValue::Counter(1))
        );
        let requested = recorded_samples(
            &metrics,
            "sequencer_get_payload_offset_seconds",
            ("trigger", trigger),
        );
        assert_eq!(requested.len(), 1);
        // The offset counts from the parent's timestamp, 200 ms before the block's own.
        assert!((before..=after).contains(&requested[0]), "getPayload offset {requested:?}");
        for name in ["sequencer_seals_total", "sequencer_get_payload_offset_seconds"] {
            assert_eq!(recorded(&metrics, name, ("trigger", other)), None);
        }
    }

    /// Returns every recorded build start offset, in seconds.
    #[cfg(feature = "metrics")]
    fn build_start_offsets(metrics: &[RecordedMetric]) -> Vec<f64> {
        let mut offsets = Vec::new();
        for (key, .., value) in metrics {
            if key.key().name() != "base_node.sequencer_build_start_offset_seconds" {
                continue;
            }
            let DebugValue::Histogram(samples) = value else {
                panic!("build start offset must be a histogram, got {value:?}");
            };
            offsets.extend(samples.iter().map(|sample| sample.into_inner()));
        }
        offsets
    }

    /// Builds block 1 on a parent whose timestamp passed about five seconds ago.
    #[cfg(feature = "metrics")]
    #[tokio::test(start_paused = true)]
    async fn build_records_when_it_was_requested_relative_to_its_parents_timestamp() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut actor = test_actor();
        let slot_start = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 5;
        let config = Arc::make_mut(&mut actor.rollup_config);
        config.block_time = 2;
        config.genesis.l2_time = slot_start;
        config.upgrades.base.denim = Some(0);
        actor.builder.rollup_config = Arc::clone(&actor.rollup_config);
        actor.builder.attributes_builder.attributes = vec![Ok(BasePayloadAttributes::default())];
        actor
            .builder
            .origin_selector
            .expect_next_l1_origin()
            .times(1)
            .return_once(|_| Ok(BlockInfo::default()));
        let since_slot_start =
            move || (UNIX_EPOCH + Duration::from_secs(slot_start)).elapsed().unwrap().as_secs_f64();
        let (at_request_tx, at_request_rx) = std::sync::mpsc::channel();
        let mut engine = MockSequencerEngineClient::new();
        engine.expect_start_build_block().times(1).return_once(move |_| {
            at_request_tx.send(since_slot_start()).unwrap();
            // Make an offset taken after the engine responds measurably later than the request.
            std::thread::sleep(Duration::from_millis(2));
            Ok(PayloadId::new([7; 8]))
        });
        actor.builder.engine_client = Arc::new(engine);
        let mut pipeline = BuildPipelineState {
            pending_build_parent: Some(L2BlockInfo::default()),
            ..Default::default()
        };
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

        let before = since_slot_start();
        actor.handle_build_tick(&mut pipeline, &mut ticker, None).await.unwrap();

        assert!(pipeline.next_payload_to_seal.is_some());
        let offsets = build_start_offsets(&snapshotter.snapshot().into_vec());
        let at_request = at_request_rx.try_recv().unwrap();
        // The offset counts from the parent's timestamp, 200 ms before the block's own.
        assert_eq!(offsets.len(), 1);
        assert!((before..=at_request).contains(&offsets[0]), "build start offset {offsets:?}");
    }

    /// A tick that cannot start a build, here for want of an L1 origin, has no build to time.
    #[cfg(feature = "metrics")]
    #[tokio::test(start_paused = true)]
    async fn deferred_build_records_no_build_start() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut actor = test_actor();
        actor
            .builder
            .origin_selector
            .expect_next_l1_origin()
            .times(1)
            .return_once(|_| Err(L1OriginSelectorError::NotEnoughData(BlockInfo::default())));
        let mut pipeline = BuildPipelineState {
            pending_build_parent: Some(L2BlockInfo::default()),
            ..Default::default()
        };
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

        actor.handle_build_tick(&mut pipeline, &mut ticker, None).await.unwrap();

        assert!(pipeline.next_payload_to_seal.is_none());
        assert_eq!(build_start_offsets(&snapshotter.snapshot().into_vec()), Vec::<f64>::new());
    }

    /// A rebuild of the hinted block reuses its payload id, but seals on its own schedule until
    /// the execution layer reports the rebuild frozen.
    #[cfg(feature = "metrics")]
    #[tokio::test(start_paused = true)]
    async fn hint_for_a_replaced_build_does_not_mark_its_rebuild() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut actor = test_actor();
        Arc::make_mut(&mut actor.rollup_config).upgrades.base.denim = Some(0);
        let (ready, mut pipeline) = ready_build();
        let parent =
            *pipeline.next_payload_to_seal.as_ref().unwrap().attributes_with_parent.parent();
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        actor.handle_payload_ready(ready, &mut pipeline, &mut ticker);
        // A stop and restart rebuilds the same block before the woken ticker is serviced.
        pipeline.next_payload_to_seal = ready_build().1.next_payload_to_seal;
        let (sealer, _, _) = valid_sealer();
        let mut engine = MockSequencerEngineClient::new();
        engine.expect_get_unsafe_head().times(1).return_once(move || Ok(parent));
        engine.expect_get_sealed_payload().times(1).return_once(move |_, _| Ok(sealer.envelope));
        actor.engine_client = Arc::new(engine);

        actor.handle_build_tick(&mut pipeline, &mut ticker, None).await.unwrap();

        let metrics = snapshotter.snapshot().into_vec();
        assert_eq!(
            recorded(&metrics, "sequencer_seals_total", ("trigger", "deadline")),
            Some(&DebugValue::Counter(1))
        );
        assert_eq!(recorded(&metrics, "sequencer_seals_total", ("trigger", "ready_hint")), None);
    }

    fn ready_build() -> (PayloadReady, BuildPipelineState) {
        let ready =
            PayloadReady { payload_id: PayloadId::new([7; 8]), parent_hash: B256::repeat_byte(8) };
        let parent = L2BlockInfo {
            block_info: BlockInfo { hash: ready.parent_hash, ..Default::default() },
            ..Default::default()
        };
        let handle = UnsealedPayloadHandle {
            payload_id: ready.payload_id,
            attributes_with_parent: AttributesWithParent::new(
                BasePayloadAttributes::default(),
                parent,
                None,
                false,
            ),
            frozen: false,
            build_requested_at: SystemTime::now(),
        };
        (ready, BuildPipelineState { next_payload_to_seal: Some(handle), ..Default::default() })
    }

    #[tokio::test(start_paused = true)]
    async fn frozen_hint_fetches_payload_before_deadline_once() {
        let mut actor = test_actor();
        Arc::make_mut(&mut actor.rollup_config).upgrades.base.denim = Some(0);
        let (ready, mut pipeline) = ready_build();
        let parent =
            *pipeline.next_payload_to_seal.as_ref().unwrap().attributes_with_parent.parent();
        let (sealer, _, _) = valid_sealer();
        let mut engine = MockSequencerEngineClient::new();
        engine.expect_get_unsafe_head().times(1).return_once(move || Ok(parent));
        engine
            .expect_get_sealed_payload()
            .times(1)
            .withf(move |id, _| *id == ready.payload_id)
            .return_once(move |_, _| Ok(sealer.envelope));
        actor.engine_client = Arc::new(engine);
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        ticker.reset_at(SystemTime::now() + Duration::from_secs(10));
        assert!(ticker.tick().now_or_never().is_none());
        actor.handle_payload_ready(ready, &mut pipeline, &mut ticker);
        assert!(ticker.tick().now_or_never().is_some());
        actor.handle_build_tick(&mut pipeline, &mut ticker, None).await.unwrap();
        assert!(actor.sealer.is_some());
        assert!(pipeline.next_payload_to_seal.is_none());
        // A duplicate cannot wake the ticker while the payload is already sealing.
        ticker.reset_at(SystemTime::now() + Duration::from_secs(10));
        actor.handle_payload_ready(ready, &mut pipeline, &mut ticker);
        assert!(ticker.tick().now_or_never().is_none());
    }

    #[rstest]
    #[case("wrong_id", "payload_id_mismatch")]
    #[case("wrong_parent", "parent_mismatch")]
    #[case("stopped", "inactive")]
    #[case("already_sealing", "already_sealing")]
    #[case("sealed_before_next_build", "already_sealing")]
    #[case("sealed_after_next_build", "already_sealing")]
    #[case("pre_denim", "pre_denim")]
    #[case("no_build", "no_build")]
    #[tokio::test(start_paused = true)]
    async fn unrelated_ready_hints_preserve_seal_deadline(
        #[case] scenario: &str,
        #[case] reason: &str,
    ) {
        #[cfg(feature = "metrics")]
        let recorder = DebuggingRecorder::new();
        #[cfg(feature = "metrics")]
        let snapshotter = recorder.snapshotter();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let mut actor = test_actor();
        Arc::make_mut(&mut actor.rollup_config).upgrades.base.denim = Some(0);
        let (mut ready, mut pipeline) = ready_build();
        match scenario {
            "wrong_id" => ready.payload_id = PayloadId::new([9; 8]),
            "wrong_parent" => ready.parent_hash = B256::repeat_byte(9),
            "stopped" => actor.is_active = false,
            "already_sealing" => actor.sealer = Some(valid_sealer().0),
            // The seal deadline fetched this payload before its hint was read.
            "sealed_before_next_build" => {
                pipeline.sealed_payload_id =
                    pipeline.next_payload_to_seal.take().map(|handle| handle.payload_id);
            }
            "sealed_after_next_build" => {
                pipeline.sealed_payload_id = Some(ready.payload_id);
                pipeline.next_payload_to_seal.as_mut().unwrap().payload_id = PayloadId::new([9; 8]);
            }
            "pre_denim" => {
                Arc::make_mut(&mut actor.rollup_config).upgrades.base = BaseUpgradeConfig::default()
            }
            "no_build" => pipeline.next_payload_to_seal = None,
            _ => unreachable!(),
        }
        assert_eq!(
            actor.ready_hint_drop_reason(ready, &pipeline).map(ReadyHintDropReason::as_str),
            Some(reason)
        );
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        ticker.reset_at(SystemTime::now() + Duration::from_secs(10));
        actor.handle_payload_ready(ready, &mut pipeline, &mut ticker);
        assert!(ticker.tick().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(ticker.tick().now_or_never().is_some());

        #[cfg(feature = "metrics")]
        {
            let dropped: Vec<_> = snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .filter(|(key, ..)| {
                    key.key().name() == "base_node.sequencer_payload_ready_hints_dropped_total"
                })
                .map(|(key, .., value)| {
                    (key.key().labels().next().unwrap().value().to_owned(), value)
                })
                .collect();
            assert_eq!(dropped, [(reason.to_owned(), DebugValue::Counter(1))]);
        }
    }

    /// Payload ids are derived from the parent and attributes, so rebuilding the block that was
    /// sealed last, as after an engine reset, reuses its id.
    #[tokio::test(start_paused = true)]
    async fn hint_for_a_rebuild_of_the_last_sealed_payload_seals_early() {
        let mut actor = test_actor();
        Arc::make_mut(&mut actor.rollup_config).upgrades.base.denim = Some(0);
        let (ready, mut pipeline) = ready_build();
        pipeline.sealed_payload_id = Some(ready.payload_id);
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        ticker.reset_at(SystemTime::now() + Duration::from_secs(10));

        actor.handle_payload_ready(ready, &mut pipeline, &mut ticker);

        assert!(ticker.tick().now_or_never().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn early_insertion_keeps_next_build_at_parent_timestamp() {
        let mut actor = test_actor();
        let future = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 30;
        Arc::make_mut(&mut actor.rollup_config).genesis.l2_time = future;
        Arc::make_mut(&mut actor.rollup_config).upgrades.base.denim = Some(0);
        let (sealer, head, _) = valid_sealer();
        actor.sealer = Some(sealer);
        let mut pipeline = BuildPipelineState::default();
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        let mut reconciliation = tokio::time::interval(Duration::from_secs(2));
        actor
            .handle_seal_step_result(
                &mut pipeline,
                &mut None,
                &mut reconciliation,
                &mut ticker,
                Ok(SealStepOutcome::Inserted(head)),
            )
            .await
            .unwrap();
        assert_eq!(pipeline.pending_build_parent, Some(head));
        assert!(ticker.tick().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(ticker.tick().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(ticker.tick().now_or_never().is_some());
    }

    fn valid_sealer() -> (PayloadSealer, L2BlockInfo, SystemConfig) {
        let block = BaseBlock {
            header: alloy_consensus::Header { number: 1, ..Default::default() },
            body: alloy_consensus::BlockBody {
                transactions: vec![BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
                    input: L1BlockInfoBedrock::default().encode_calldata(),
                    ..Default::default()
                }))],
                ..Default::default()
            },
        };
        let (payload, _) = BaseExecutionPayload::from_block_slow(&block);
        let config = to_system_config_from_payload(&payload, &RollupConfig::default()).unwrap();
        let head = L2BlockInfo {
            block_info: BlockInfo {
                hash: payload.block_hash(),
                number: payload.block_number(),
                ..Default::default()
            },
            ..Default::default()
        };
        let envelope = BaseExecutionPayloadEnvelope {
            execution_payload: payload,
            parent_beacon_block_root: None,
        };
        (PayloadSealer::new(envelope), head, config)
    }

    #[tokio::test]
    async fn acknowledged_payload_seeds_only_an_exact_decodable_parent() {
        let (sealer, head, config) = valid_sealer();
        let mut actor = test_actor();
        actor.sealer = Some(sealer);
        let mut pipeline = BuildPipelineState::default();
        let mut shadow = None;
        let mut reconciliation_ticker = tokio::time::interval(Duration::from_secs(1));
        let mut build_ticker = ScheduledTicker::new(Duration::from_secs(2));

        actor
            .handle_seal_step_result(
                &mut pipeline,
                &mut shadow,
                &mut reconciliation_ticker,
                &mut build_ticker,
                Ok(SealStepOutcome::Inserted(head)),
            )
            .await
            .unwrap();
        assert_eq!(actor.builder.attributes_builder.parent_system_configs, vec![(head, config)]);

        let (sealer, mut mismatched_head, _) = valid_sealer();
        mismatched_head.block_info.hash = B256::left_padding_from(&[1]);
        actor.sealer = Some(sealer);
        actor
            .handle_seal_step_result(
                &mut pipeline,
                &mut shadow,
                &mut reconciliation_ticker,
                &mut build_ticker,
                Ok(SealStepOutcome::Inserted(mismatched_head)),
            )
            .await
            .unwrap();
        assert_eq!(actor.builder.attributes_builder.parent_system_configs.len(), 1);

        let (mut sealer, head, _) = valid_sealer();
        sealer.envelope.execution_payload.transactions_mut().clear();
        actor.sealer = Some(sealer);
        actor
            .handle_seal_step_result(
                &mut pipeline,
                &mut shadow,
                &mut reconciliation_ticker,
                &mut build_ticker,
                Ok(SealStepOutcome::Inserted(head)),
            )
            .await
            .unwrap();
        assert_eq!(actor.builder.attributes_builder.parent_system_configs.len(), 1);
    }
}
