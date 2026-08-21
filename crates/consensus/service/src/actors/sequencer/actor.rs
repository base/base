//! The [`SequencerActor`].

use std::{
    num::NonZeroU64,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_consensus_derive::AttributesBuilder;
use base_consensus_rpc::SequencerAdminAPIError;
use base_protocol::L2BlockInfo;
use tokio::{
    select,
    sync::{mpsc, oneshot},
    task::JoinError,
    time::Interval,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, Metrics, NodeActor, ResetReason, SequencerAdminQuery,
    UnsafePayloadGossipClient,
    actors::{
        SequencerEngineClient,
        engine::{EngineClientError, EngineClientResult},
        sequencer::{
            BuildPipelineState, ScheduledTicker, ShadowSequencingState,
            build::{BuildOutcome, PayloadBuilder, UnsealedPayloadHandle},
            conductor::Conductor,
            error::SequencerActorError,
            l1_origin::OriginSelector,
            recovery::RecoveryModeGuard,
            seal::{PayloadSealer, SealStepError, SealStepOutcome},
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
    /// Number of private blocks to build per shadow sequencing cycle.
    pub shadow_blocks_per_cycle: Option<NonZeroU64>,
    /// Optional account funding injected into the first private block of every shadow cycle.
    pub shadow_funding: Option<ShadowFunding>,
    /// Shared recovery mode flag.
    pub recovery_mode: RecoveryModeGuard,
    /// The rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
    /// Fixed offset into each subsecond slot at which the sealed payload is requested from
    /// the engine once Denim is active. See [`SequencerConfig::seal_offset`].
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
        self.shadow_blocks_per_cycle.is_some()
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

        if self.is_shadow_sequencer() {
            Ok(PayloadSealer::new_private(envelope))
        } else {
            Ok(PayloadSealer::new(envelope))
        }
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
    async fn schedule_initial_reset(
        &mut self,
        next_payload: &mut Option<UnsealedPayloadHandle>,
    ) -> Result<(), SequencerActorError> {
        loop {
            let engine_client = Arc::clone(&self.engine_client);
            let shadow_cycle_coordinated = self.is_shadow_sequencer();
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => return Ok(()),
                Some(query) = self.admin_api_rx.recv() => {
                    self.handle_admin_query(next_payload, query).await;
                }
                result = async {
                    if shadow_cycle_coordinated {
                        engine_client
                            .reset_engine_forkchoice_coordinated(ResetReason::ShadowCycle)
                            .await
                    } else {
                        engine_client
                            .reset_engine_forkchoice(ResetReason::SequencerStartup)
                            .await
                    }
                } => match result {
                    Ok(()) => return Ok(()),
                    Err(EngineClientError::ELSyncing) => {
                        info!(target: "sequencer", "EL sync or canonical catch-up in progress; deferring initial engine reset");
                    }
                    Err(err) => {
                        error!(target: "sequencer", error = ?err, "Failed to send reset request to engine");
                        return Err(err.into());
                    }
                },
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
                        self.handle_admin_query(next_payload, query).await;
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
            Self::apply_eager_build_outcome(outcome, pipeline, build_ticker);
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
                self.shadow_blocks_per_cycle.expect("shadow mode checked").get(),
            )
            .inspect_err(|_| self.cancellation_token.cancel())
    }

    /// Wall-clock time at which `block_number` should be sealed.
    ///
    /// Denim-active blocks seal at a fixed offset into their 200ms slot, computed as
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
                    Self::apply_eager_build_outcome(outcome, pipeline, build_ticker);
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
        outcome: BuildOutcome<UnsealedPayloadHandle>,
        pipeline: &mut BuildPipelineState,
        build_ticker: &mut ScheduledTicker,
    ) {
        match outcome {
            BuildOutcome::Ready(payload) => {
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
        match self.try_seal_handle(handle).await? {
            Some((new_sealer, dur)) => {
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
