//! Sequencer ownership and serialized routing of engine requests.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_consensus_engine::{
    ConsolidateTask, EngineClient, EngineTask, EngineTaskError, EngineTaskErrorSeverity,
    EngineTaskErrors, FinalizeTask, Metrics as EngineMetrics, SealTaskError,
};
use opentelemetry::context::FutureExt as OtelFutureExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{self, Instant as TokioInstant, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

use super::{CanonicalUnsafeCatchup, Conductor, SequencerEngineState, ShadowReconciliationGate};
use crate::{
    BuildRequest, EngineActorRequest, EngineClientError, EngineError, EngineProcessor,
    EngineRequestReceiver, GetPayloadRequest, InsertUnsafePayloadRequest, Metrics,
    ReconcileShadowRequest, ResetOrigin, ResetRequest, ResetRequestOutcome,
    actors::engine::ResetOutcome,
};

const MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP: u64 = 300;
const SEQUENCER_EL_SYNC_PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapRole {
    ActiveSequencer,
    ConductorFollower,
}

/// Owns the engine processor and routes sequencer catch-up and shadow reconciliation requests.
#[derive(Debug)]
pub struct SequencerEngineRequestCoordinator<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    processor: EngineProcessor<EngineClient_>,
    /// Canonical catch-up or active shadow reconciliation state.
    sequencer_state: SequencerEngineState,
    conductor: Option<Arc<dyn Conductor>>,
    sequencer_stopped: bool,
    unsafe_head_tx: watch::Sender<base_protocol::L2BlockInfo>,
}

impl<EngineClient_> SequencerEngineRequestCoordinator<EngineClient_>
where
    EngineClient_: EngineClient,
{
    /// Creates a request handler with optional shadow request routing.
    pub fn new(
        processor: EngineProcessor<EngineClient_>,
        shadow_mode: bool,
        conductor: Option<Arc<dyn Conductor>>,
        sequencer_stopped: bool,
        unsafe_head_tx: watch::Sender<base_protocol::L2BlockInfo>,
    ) -> Self {
        let sequencer_state = if shadow_mode {
            SequencerEngineState::CatchingUp {
                shadow: true,
                catchup: CanonicalUnsafeCatchup::default(),
            }
        } else {
            SequencerEngineState::Regular
        };
        Self { processor, sequencer_state, conductor, sequencer_stopped, unsafe_head_tx }
    }

    /// Returns the coordinator's sequencer routing state.
    pub const fn sequencer_state(&self) -> &SequencerEngineState {
        &self.sequencer_state
    }

    /// Returns mutable sequencer routing state for test harness setup.
    pub const fn sequencer_state_mut(&mut self) -> &mut SequencerEngineState {
        &mut self.sequencer_state
    }

    /// Validates an authorized admin start against the serialized engine state.
    ///
    /// Conductor supplies its committed head to admin start even when it has no new payload to
    /// post. That head can release an unobserved non-shadow catch-up, but cannot override known
    /// gaps or conflicts. The caller must check conductor leadership before requesting activation.
    /// Shadow starts validate the head without changing catch-up or reconciliation state.
    pub fn prepare_sequencer_start(
        &mut self,
        expected_hash: B256,
    ) -> Result<(), EngineClientError> {
        let sync_state = self.processor.engine_state().sync_state;
        let head = sync_state.unsafe_head();
        if expected_hash == B256::ZERO || head.block_info.hash == B256::ZERO {
            return Err(EngineClientError::RequestError(
                "no prestate: unsafe head is uninitialized".to_string(),
            ));
        }
        if expected_hash != head.block_info.hash {
            return Err(EngineClientError::RequestError(format!(
                "block hash mismatch: engine unsafe head is {}, caller requested {}",
                head.block_info.hash, expected_hash
            )));
        }

        // Shadow activation must not release its catch-up or reconciliation gate.
        if self.is_shadow_sequencer() {
            return Ok(());
        }

        if !self.processor.engine_state().el_sync_finished {
            return Err(EngineClientError::ELSyncing);
        }

        if let SequencerEngineState::CatchingUp { catchup, .. } = &self.sequencer_state {
            if catchup.is_faulted() {
                return Err(EngineClientError::ShadowBufferFaulted);
            }

            if catchup.has_observations() && !catchup.is_complete(head, sync_state.safe_head()) {
                return Err(EngineClientError::ELSyncing);
            }
        }

        self.sequencer_state = SequencerEngineState::Regular;
        Ok(())
    }

    /// Returns whether this handler is configured as a shadow sequencer.
    const fn is_shadow_sequencer(&self) -> bool {
        matches!(
            self.sequencer_state,
            SequencerEngineState::CatchingUp { shadow: true, .. }
                | SequencerEngineState::ShadowActive(_)
        )
    }

    const fn active_shadow_gate(&mut self) -> Option<&mut ShadowReconciliationGate> {
        match &mut self.sequencer_state {
            SequencerEngineState::ShadowActive(gate) => Some(gate),
            _ => None,
        }
    }

    const fn is_shadow_active(&self) -> bool {
        matches!(self.sequencer_state, SequencerEngineState::ShadowActive(_))
    }

    async fn resolve_bootstrap_role(&self) -> BootstrapRole {
        if self.sequencer_stopped || self.is_shadow_sequencer() {
            return BootstrapRole::ConductorFollower;
        }
        match &self.conductor {
            None => BootstrapRole::ActiveSequencer,
            Some(conductor) => match conductor.leader().await {
                Ok(true) => BootstrapRole::ActiveSequencer,
                Ok(false) => BootstrapRole::ConductorFollower,
                Err(err) => {
                    warn!(target: "engine", error = %err, "Bootstrap: conductor leadership check failed, assuming follower");
                    BootstrapRole::ConductorFollower
                }
            },
        }
    }

    async fn advance_canonical_catchup(&mut self) -> Result<(), EngineError> {
        let anchor = self.processor.engine_state().sync_state.unsafe_head();
        let payloads = match &mut self.sequencer_state {
            SequencerEngineState::CatchingUp { catchup, .. } => catchup.contiguous_payloads(anchor),
            _ => return Ok(()),
        };
        if payloads.is_empty() {
            return Ok(());
        }

        debug!(
            target: "engine",
            anchor = anchor.block_info.number,
            payload_count = payloads.len(),
            first_payload = payloads.first().map(|payload| payload.execution_payload.block_number()),
            last_payload = payloads.last().map(|payload| payload.execution_payload.block_number()),
            "Applying ordered canonical unsafe payloads"
        );
        let client = Arc::clone(self.processor.client());
        let rollup = Arc::clone(self.processor.rollup());
        match self
            .processor
            .engine_mut()
            .insert_authoritative_payloads(client, rollup, payloads)
            .await
        {
            Ok(head) => {
                if let SequencerEngineState::CatchingUp { catchup, .. } = &mut self.sequencer_state
                {
                    catchup.commit(head);
                }
            }
            Err(error) => {
                debug!(
                    target: "engine",
                    error = ?error,
                    severity = ?error.severity(),
                    "Ordered canonical unsafe payload application deferred"
                );
                return match error.severity() {
                    EngineTaskErrorSeverity::Temporary | EngineTaskErrorSeverity::Deferred => {
                        Ok(())
                    }
                    _ => Err(EngineError::CriticalEngineTask(format!("{error:?}"))),
                };
            }
        }
        Ok(())
    }
}

impl<EngineClient_> EngineRequestReceiver for SequencerEngineRequestCoordinator<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    fn start(
        mut self,
        mut request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        tokio::spawn(async move {
            // Bootstrap: pre-populate the unsafe_head_tx watch channel so that external callers
            // (admin_startSequencer, sync-status RPC) never observe a zero hash.
            //
            // We gate on whether reth's current head is at the rollup genesis:
            //
            //   • At genesis — reth has no snap-synced canonical chain, so engine.reset() is
            //     safe: it FCUs to the genesis block and sets up derivation normally. The
            //     el_sync_finished / el_sync_complete gate is preserved as before.
            //
            //   • Beyond genesis — reth already has a canonical chain (e.g. after snap sync).
            //     Sending a FCU to the sync-start block would reorg reth below its state pivot,
            //     causing every subsequent engine_newPayload to return Syncing and the node to
            //     enter an infinite reset loop. Instead we seed the watch channel from reth's
            //     current head directly; derivation will issue its own FCU once the first Reset
            //     task arrives.
            let opt_head = self
                .processor
                .client()
                .l2_block_info_by_label(BlockNumberOrTag::Latest)
                .await
                .map_err(|err| {
                    error!(target: "engine", ?err, "Bootstrap: failed to query reth head");
                    EngineError::BootstrapHeadQuery(err.to_string())
                })?;
            let at_genesis = opt_head
                .is_none_or(|head| head.block_info.hash == self.processor.rollup().genesis.l2.hash);

            let bootstrap_role = self.resolve_bootstrap_role().await;
            if bootstrap_role == BootstrapRole::ConductorFollower
                && !at_genesis
                && matches!(self.sequencer_state, SequencerEngineState::Regular)
            {
                self.sequencer_state = SequencerEngineState::CatchingUp {
                    shadow: false,
                    catchup: CanonicalUnsafeCatchup::default(),
                };
            }
            match bootstrap_role {
                BootstrapRole::ConductorFollower => {
                    self.processor.bootstrap_conductor_follower(opt_head).await
                }
                BootstrapRole::ActiveSequencer => {
                    self.processor.bootstrap_active_sequencer(opt_head, at_genesis).await
                }
            }

            let mut el_sync_probe_interval = time::interval_at(
                TokioInstant::now() + SEQUENCER_EL_SYNC_PROBE_INTERVAL,
                SEQUENCER_EL_SYNC_PROBE_INTERVAL,
            );
            el_sync_probe_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                // Full processor iteration window: drain + recv wait + request handling.
                // Bounds the worst-case channel wait — any request arriving during this
                // iteration waits at most this long before the next recv picks it up.
                let _iter_timer =
                    base_metrics::timed!(EngineMetrics::engine_processor_iteration_duration());

                // Attempt to drain all outstanding tasks from the engine queue before adding new
                // ones.
                let drain_outcome = base_metrics::time!(
                    EngineMetrics::engine_processor_drain_duration_seconds(),
                    {
                        self.processor.drain().await.inspect_err(
                            |err| error!(target: "engine", ?err, "Failed to drain engine tasks"),
                        )
                    }
                )?;
                // A genuine drain reset invalidates shadow reconciliation state and is fatal. The
                // one-time `InitialELSyncReset` (a cold-start bootstrap reset) is deliberately
                // tolerated: it carries no reconciliation state and cannot recur.
                if drain_outcome == ResetOutcome::Reset && self.is_shadow_active() {
                    return Err(EngineError::ShadowInternalReset);
                }
                if drain_outcome == ResetOutcome::Reset
                    && let SequencerEngineState::CatchingUp { catchup, .. } =
                        &mut self.sequencer_state
                {
                    *catchup = CanonicalUnsafeCatchup::default();
                }

                self.advance_canonical_catchup().await?;

                // If the unsafe head has updated, propagate it to the outbound channels.
                self.unsafe_head_tx.send_if_modified(|val| {
                    let new_head = self.processor.engine_state().sync_state.unsafe_head();
                    (*val != new_head).then(|| *val = new_head).is_some()
                });

                // Wait for the next processing request.
                let recv_wait_timer = base_metrics::timed!(
                    EngineMetrics::engine_processor_recv_wait_duration_seconds()
                );
                let recv_result = if self.processor.engine_state().el_sync_finished {
                    request_channel.recv().await
                } else {
                    tokio::select! {
                        request = request_channel.recv() => request,
                        _ = el_sync_probe_interval.tick() => {
                            drop(recv_wait_timer);
                            let active_sequencer = self.resolve_bootstrap_role().await
                                == BootstrapRole::ActiveSequencer;
                            self.processor
                                .probe_sequencer_el_sync(active_sequencer)
                                .await;
                            continue;
                        }
                    }
                };
                drop(recv_wait_timer);
                let Some(request) = recv_result else {
                    error!(target: "engine", "Engine processing request receiver closed unexpectedly");
                    return Err(EngineError::ChannelClosed);
                };

                match request {
                    EngineActorRequest::PrepareSequencerStart { expected_hash, result_tx } => {
                        let result = self.prepare_sequencer_start(expected_hash);
                        if result_tx.send(result).await.is_err() {
                            warn!(target: "engine", "Sequencer start response receiver dropped");
                        }
                    }
                    EngineActorRequest::BuildRequest(build_request) => {
                        let BuildRequest { attributes, result_tx, otel_cx } = *build_request;
                        let client = Arc::clone(self.processor.client());
                        let rollup = Arc::clone(self.processor.rollup());
                        let build_result = self
                            .processor
                            .engine_mut()
                            .build(client, rollup, attributes)
                            .with_context(otel_cx)
                            .await;
                        match build_result {
                            Ok(payload_id) => {
                                result_tx
                                    .send(Ok(payload_id))
                                    .await
                                    .map_err(|_| EngineError::ChannelClosed)?;
                            }
                            Err(err) => {
                                let severity = err.severity();
                                let error = format!("{err:?}");
                                result_tx
                                    .send(Err(err))
                                    .await
                                    .map_err(|_| EngineError::ChannelClosed)?;
                                let outcome = self
                                    .processor
                                    .handle_engine_task_error_severity(severity, error)
                                    .await?;
                                if self.is_shadow_active() && outcome == ResetOutcome::Reset {
                                    return Err(EngineError::ShadowInternalReset);
                                }
                            }
                        }
                    }
                    EngineActorRequest::GetPayloadRequest(get_payload_request) => {
                        let GetPayloadRequest { payload_id, attributes, result_tx, otel_cx } =
                            *get_payload_request;
                        let client = Arc::clone(self.processor.client());
                        let rollup = Arc::clone(self.processor.rollup());
                        let result = self
                            .processor
                            .engine_mut()
                            .get_payload(client, rollup, payload_id, attributes)
                            .with_context(otel_cx)
                            .await;

                        let error =
                            result.as_ref().err().map(|err| (err.severity(), format!("{err:?}")));
                        result_tx.send(result).await.map_err(|err| {
                            EngineTaskErrors::Seal(SealTaskError::MpscSend(Box::new(err)))
                        })?;
                        if let Some((severity, error)) = error {
                            let outcome = self
                                .processor
                                .handle_engine_task_error_severity(severity, error)
                                .await?;
                            if self.is_shadow_active() && outcome == ResetOutcome::Reset {
                                return Err(EngineError::ShadowInternalReset);
                            }
                        }
                    }
                    EngineActorRequest::ProcessSafeL2SignalRequest(safe_signal) => {
                        // Canonical ancestors of the cycle anchor cannot reorg the private unsafe
                        // branch, so confirm them immediately and let derivation keep advancing.
                        let should_defer = self
                            .active_shadow_gate()
                            .is_some_and(|gate| gate.should_defer_safe_signal(&safe_signal));
                        if should_defer {
                            self.active_shadow_gate()
                                .expect("gate checked")
                                .buffer_safe_signal(safe_signal);
                            continue;
                        }
                        let task = EngineTask::Consolidate(Box::new(ConsolidateTask::new(
                            Arc::clone(self.processor.client()),
                            Arc::clone(self.processor.rollup()),
                            safe_signal,
                        )));
                        self.processor.enqueue(task);
                    }
                    EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(
                        finalized_l2_block_number,
                    ) => {
                        // Finalization is equally safe through the anchor once the corresponding
                        // block is safe; applying it promptly also keeps ExEx WAL pruning moving.
                        let safe_head_number =
                            self.processor.engine_state().sync_state.safe_head().block_info.number;
                        let should_defer = self.active_shadow_gate().is_some_and(|gate| {
                            gate.should_defer_finalized(
                                *finalized_l2_block_number,
                                safe_head_number,
                            )
                        });
                        if should_defer {
                            self.active_shadow_gate()
                                .expect("gate checked")
                                .buffer_finalized(*finalized_l2_block_number);
                            continue;
                        }
                        // Finalize the L2 block at the provided block number.
                        let task = EngineTask::Finalize(Box::new(FinalizeTask::new(
                            Arc::clone(self.processor.client()),
                            Arc::clone(self.processor.rollup()),
                            *finalized_l2_block_number,
                        )));
                        self.processor.enqueue(task);
                    }
                    EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                        match &mut self.sequencer_state {
                            SequencerEngineState::CatchingUp { catchup, .. } => {
                                catchup.buffer_payload(*envelope);
                            }
                            SequencerEngineState::ShadowActive(gate) => {
                                gate.buffer_payload(*envelope);
                            }
                            SequencerEngineState::Regular => {
                                let block_number = envelope.execution_payload.block_number();
                                let unsafe_head =
                                    self.processor.engine_state().sync_state.unsafe_head();
                                let block_gap =
                                    block_number.checked_sub(unsafe_head.block_info.number);
                                if block_gap.is_some_and(|gap| {
                                    gap > 0 && gap <= MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP
                                }) {
                                    info!(
                                        target: "engine",
                                        block_number,
                                        block_hash = %envelope.execution_payload.block_hash(),
                                        parent_hash = %envelope.execution_payload.parent_hash(),
                                        block_gap = ?block_gap,
                                        max_external_unsafe_gap = MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP,
                                        "Sequencer enqueuing external unsafe payload within gap limit"
                                    );
                                    self.processor
                                        .enqueue_unsafe_payload_insert(*envelope, None, false);
                                } else {
                                    info!(
                                        target: "engine",
                                        block_number,
                                        block_hash = %envelope.execution_payload.block_hash(),
                                        parent_hash = %envelope.execution_payload.parent_hash(),
                                        block_gap = ?block_gap,
                                        max_external_unsafe_gap = MAX_SEQUENCER_EXTERNAL_UNSAFE_GAP,
                                        unsafe_head_number = unsafe_head.block_info.number,
                                        unsafe_head_hash = %unsafe_head.block_info.hash,
                                        "Sequencer dropped external unsafe payload outside gap limit"
                                    );
                                }
                            }
                        }
                    }
                    EngineActorRequest::ProcessAdminUnsafeL2BlockRequest(envelope) => {
                        match self.sequencer_state {
                            SequencerEngineState::CatchingUp { shadow: true, .. } => {
                                warn!(target: "engine", "Ignoring admin unsafe payload during canonical catch-up");
                            }
                            SequencerEngineState::ShadowActive(_) => {
                                warn!(target: "engine", "Ignoring admin unsafe payload on shadow sequencer");
                            }
                            SequencerEngineState::Regular
                            | SequencerEngineState::CatchingUp { shadow: false, .. } => {
                                self.processor.handle_admin_unsafe_l2_block(*envelope);
                            }
                        }
                    }
                    EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(envelope) => {
                        let InsertUnsafePayloadRequest { envelope, result_tx, otel_cx } = *envelope;
                        // Attach for the synchronous enqueue call only — no await, no Send issue.
                        let _guard = otel_cx.attach();
                        if let Some(gate) = self.active_shadow_gate() {
                            gate.buffer_local_payload(&envelope);
                        }
                        self.processor.handle_local_unsafe_l2_block(envelope, result_tx);
                    }
                    EngineActorRequest::ReconcileShadowRequest(request) => {
                        let ReconcileShadowRequest { shadow_head, result_tx } = *request;
                        let result = match &mut self.sequencer_state {
                            SequencerEngineState::ShadowActive(gate) => {
                                match gate.prepare(shadow_head) {
                                    Ok(Some(inputs)) => {
                                        self.processor.apply_canonical_inputs(inputs).await
                                    }
                                    Ok(None) => Ok(None),
                                    Err(error) => Err(error),
                                }
                            }
                            SequencerEngineState::Regular
                            | SequencerEngineState::CatchingUp { .. } => {
                                Err(EngineClientError::ShadowReconciliationDisabled)
                            }
                        };
                        if let Ok(Some(head)) = &result {
                            self.active_shadow_gate().expect("gate checked").commit(*head);
                        }
                        let failure = result.as_ref().err().map(ToString::to_string);
                        if result_tx.send(result).await.is_err() {
                            warn!(target: "engine", "Shadow reconciliation response receiver dropped");
                        }
                        if let Some(error) = failure {
                            return Err(EngineError::ShadowReconciliationFailed(error));
                        }
                    }
                    EngineActorRequest::ResetRequest(reset_request) => {
                        let reset_started = Instant::now();
                        let ResetRequest { result_tx, origin, reason } = *reset_request;
                        let sync_state = self.processor.engine_state().sync_state;
                        let head = sync_state.unsafe_head();
                        let unsafe_before = head;
                        if origin != ResetOrigin::Derivation
                            && let SequencerEngineState::CatchingUp { shadow, catchup } =
                                &self.sequencer_state
                        {
                            let shadow = *shadow;
                            if catchup.is_faulted() {
                                error!(target: "engine", "Canonical catch-up payload buffer is faulted");
                                Metrics::record_engine_reset(
                                    origin,
                                    reason,
                                    ResetRequestOutcome::Failed,
                                    reset_started.elapsed(),
                                    unsafe_before,
                                    self.processor.engine_state().sync_state.unsafe_head(),
                                );
                                if result_tx
                                    .send(Err(EngineClientError::ShadowBufferFaulted))
                                    .await
                                    .is_err()
                                {
                                    warn!(target: "engine", "Sending catch-up fault response failed");
                                }
                                continue;
                            }
                            if !catchup.is_complete(head, sync_state.safe_head()) {
                                warn!(target: "engine", "Deferring sequencer reset until canonical catch-up completes");
                                Metrics::record_engine_reset(
                                    origin,
                                    reason,
                                    ResetRequestOutcome::Deferred,
                                    reset_started.elapsed(),
                                    unsafe_before,
                                    self.processor.engine_state().sync_state.unsafe_head(),
                                );
                                if result_tx.send(Err(EngineClientError::ELSyncing)).await.is_err()
                                {
                                    warn!(target: "engine", "Sending ELSyncing response failed");
                                }
                                continue;
                            }
                            if shadow {
                                if origin != ResetOrigin::ShadowCycleCoordinated {
                                    Metrics::record_engine_reset(
                                        origin,
                                        reason,
                                        ResetRequestOutcome::Failed,
                                        reset_started.elapsed(),
                                        unsafe_before,
                                        self.processor.engine_state().sync_state.unsafe_head(),
                                    );
                                    if result_tx
                                        .send(Err(EngineClientError::ShadowReconciliationDisabled))
                                        .await
                                        .is_err()
                                    {
                                        warn!(target: "engine", "Sending shadow activation response failed");
                                    }
                                    continue;
                                }
                                info!(
                                    target: "engine",
                                    canonical_head = head.block_info.number,
                                    canonical_hash = %head.block_info.hash,
                                    "Shadow canonical catch-up completed"
                                );
                                // Catch-up already advanced the EL to this canonical head. This
                                // coordinated request activates shadow production rather than
                                // issuing a second forkchoice reset that could rewind it.
                                self.sequencer_state = SequencerEngineState::ShadowActive(
                                    Box::new(ShadowReconciliationGate::new(head)),
                                );
                                self.unsafe_head_tx.send_replace(head);
                                Metrics::record_engine_reset(
                                    origin,
                                    reason,
                                    ResetRequestOutcome::from_unsafe_heads(unsafe_before, head),
                                    reset_started.elapsed(),
                                    unsafe_before,
                                    head,
                                );
                                if result_tx.send(Ok(())).await.is_err() {
                                    warn!(target: "engine", "Sending shadow activation response failed");
                                }
                                continue;
                            }
                            info!(
                                target: "engine",
                                canonical_head = head.block_info.number,
                                canonical_hash = %head.block_info.hash,
                                "Sequencer canonical catch-up completed"
                            );
                            self.sequencer_state = SequencerEngineState::Regular;
                        }
                        // Do not reset the engine while the EL is still syncing. A Reset sends a
                        // forkchoice_updated to reth pointing at the sync-start block, which will
                        // return Valid and cause reth to set that stale block as canonical,
                        // aborting any in-progress snap sync. Defer until el_sync_finished=true.
                        if !self.processor.engine_state().el_sync_finished {
                            warn!(target: "engine", "Deferring engine reset: EL sync not yet complete");
                            Metrics::record_engine_reset(
                                origin,
                                reason,
                                ResetRequestOutcome::Deferred,
                                reset_started.elapsed(),
                                unsafe_before,
                                self.processor.engine_state().sync_state.unsafe_head(),
                            );
                            if result_tx.send(Err(EngineClientError::ELSyncing)).await.is_err() {
                                warn!(target: "engine", "Sending ELSyncing response failed");
                            }
                            continue;
                        }

                        warn!(target: "engine", "Received reset request");

                        let reset_res = self.processor.reset_engine_state().await;
                        if let Ok(safe_head) = &reset_res {
                            let anchor = self.processor.engine_state().sync_state.unsafe_head();
                            match &mut self.sequencer_state {
                                SequencerEngineState::ShadowActive(gate) => gate.reanchor(anchor),
                                SequencerEngineState::CatchingUp { catchup, .. } => {
                                    *catchup = CanonicalUnsafeCatchup::default();
                                }
                                SequencerEngineState::Regular => {}
                            }
                            self.unsafe_head_tx.send_replace(anchor);
                            if let Err(error) =
                                self.processor.notify_derivation_of_reset(*safe_head).await
                            {
                                Metrics::record_engine_reset(
                                    origin,
                                    reason,
                                    ResetRequestOutcome::DerivationNotificationFailed,
                                    reset_started.elapsed(),
                                    unsafe_before,
                                    self.processor.engine_state().sync_state.unsafe_head(),
                                );
                                if result_tx
                                    .send(Err(EngineClientError::ResetForkchoiceError(
                                        error.to_string(),
                                    )))
                                    .await
                                    .is_err()
                                {
                                    warn!(target: "engine", "Sending reset response failed");
                                }
                                if self.is_shadow_active() {
                                    return Err(error);
                                }
                                continue;
                            }
                        }

                        let unsafe_after = self.processor.engine_state().sync_state.unsafe_head();
                        let reset_outcome = if reset_res.is_ok() {
                            ResetRequestOutcome::from_unsafe_heads(unsafe_before, unsafe_after)
                        } else {
                            ResetRequestOutcome::Failed
                        };
                        Metrics::record_engine_reset(
                            origin,
                            reason,
                            reset_outcome,
                            reset_started.elapsed(),
                            unsafe_before,
                            unsafe_after,
                        );

                        // Send the result.
                        let response_payload = reset_res
                            .as_ref()
                            .map(|_| ())
                            .map_err(|e| EngineClientError::ResetForkchoiceError(e.to_string()));
                        let reset_succeeded = reset_res.is_ok();
                        if result_tx.send(response_payload).await.is_err() {
                            warn!(target: "engine", "Sending reset response failed");
                            // If there was an error and we couldn't notify the caller to handle it,
                            // return the error.
                            reset_res?;
                        }
                        if reset_succeeded
                            && self.is_shadow_active()
                            && origin != ResetOrigin::ShadowCycleCoordinated
                        {
                            return Err(EngineError::ShadowInternalReset);
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::{
        Engine, EngineState,
        test_utils::{MockEngineClient, TestEngineStateBuilder, test_engine_client_builder},
    };
    use base_protocol::{BlockInfo, L2BlockInfo};
    use jsonrpsee::core::ClientError;
    use rstest::rstest;
    use tokio::sync::watch;

    use super::{BootstrapRole, SequencerEngineRequestCoordinator, SequencerEngineState};
    use crate::{
        CanonicalUnsafeCatchup, Conductor, ConductorError, EngineProcessor, MockConductor,
        MockEngineDerivationClient, ShadowReconciliationGate,
    };

    fn coordinator(
        shadow: bool,
        stopped: bool,
        conductor: Option<Arc<dyn Conductor>>,
    ) -> SequencerEngineRequestCoordinator<MockEngineClient> {
        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);
        let processor = EngineProcessor::new(
            Arc::new(test_engine_client_builder().build()),
            Arc::new(RollupConfig::default()),
            Box::new(MockEngineDerivationClient::new()),
            engine,
        );
        let (unsafe_head_tx, _) = watch::channel(L2BlockInfo::default());
        SequencerEngineRequestCoordinator::new(
            processor,
            shadow,
            conductor,
            stopped,
            unsafe_head_tx,
        )
    }

    #[tokio::test]
    async fn active_sequencer_without_conductor_starts_regular() {
        let coordinator = coordinator(false, false, None);
        assert!(matches!(coordinator.sequencer_state(), SequencerEngineState::Regular));
        assert_eq!(coordinator.resolve_bootstrap_role().await, BootstrapRole::ActiveSequencer);
    }

    #[tokio::test]
    async fn stopped_sequencer_skips_conductor_check() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().never();
        let coordinator = coordinator(false, true, Some(Arc::new(conductor)));
        assert_eq!(coordinator.resolve_bootstrap_role().await, BootstrapRole::ConductorFollower);
    }

    #[tokio::test]
    async fn shadow_sequencer_skips_conductor_check() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().never();
        let coordinator = coordinator(true, false, Some(Arc::new(conductor)));
        assert_eq!(coordinator.resolve_bootstrap_role().await, BootstrapRole::ConductorFollower);
    }

    #[tokio::test]
    async fn conductor_leader_is_active() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().once().returning(|| Ok(true));
        let coordinator = coordinator(false, false, Some(Arc::new(conductor)));
        assert_eq!(coordinator.resolve_bootstrap_role().await, BootstrapRole::ActiveSequencer);
    }

    #[tokio::test]
    async fn conductor_follower_is_follower() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().once().returning(|| Ok(false));
        let coordinator = coordinator(false, false, Some(Arc::new(conductor)));
        assert_eq!(coordinator.resolve_bootstrap_role().await, BootstrapRole::ConductorFollower);
    }

    #[tokio::test]
    async fn conductor_error_falls_back_to_follower() {
        let mut conductor = MockConductor::new();
        conductor
            .expect_leader()
            .once()
            .returning(|| Err(ConductorError::Rpc(ClientError::Custom("timeout".into()))));
        let coordinator = coordinator(false, false, Some(Arc::new(conductor)));
        assert_eq!(coordinator.resolve_bootstrap_role().await, BootstrapRole::ConductorFollower);
    }

    #[rstest]
    #[case::no_observations(100, true, &[], false, true)]
    #[case::caught_up(100, true, &[(100, 100)], false, true)]
    #[case::wrong_hash(99, true, &[], false, false)]
    #[case::zero_hash(0, true, &[], false, false)]
    #[case::el_syncing(100, false, &[], false, false)]
    #[case::gap(100, true, &[(102, 102)], false, false)]
    #[case::different_branch(100, true, &[(100, 99)], false, false)]
    #[case::private_tail(100, true, &[(99, 99)], false, false)]
    #[case::faulted(100, true, &[(100, 100), (100, 99)], false, false)]
    #[case::shadow_catchup(100, true, &[], true, true)]
    #[case::shadow_caught_up(100, true, &[(100, 100)], true, true)]
    #[case::shadow_el_syncing(100, false, &[], true, true)]
    #[case::shadow_gap(100, true, &[(102, 102)], true, true)]
    #[case::shadow_faulted(100, true, &[(100, 100), (100, 99)], true, true)]
    #[case::shadow_wrong_hash(99, true, &[], true, false)]
    #[case::shadow_zero_hash(0, true, &[], true, false)]
    fn prepare_start_preserves_catchup_safety(
        #[case] requested_hash: u8,
        #[case] el_synced: bool,
        #[case] observations: &[(u64, u8)],
        #[case] shadow: bool,
        #[case] accepted: bool,
    ) {
        let head = L2BlockInfo {
            block_info: BlockInfo {
                number: 100,
                hash: B256::with_last_byte(100),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = TestEngineStateBuilder::new()
            .with_unsafe_head(head)
            .with_safe_head(L2BlockInfo::default())
            .with_el_sync_finished(el_synced)
            .build();
        let (state_tx, _) = watch::channel(state);
        let (queue_tx, _) = watch::channel(0usize);
        let processor = EngineProcessor::new(
            Arc::new(test_engine_client_builder().build()),
            Arc::new(RollupConfig::default()),
            MockEngineDerivationClient::new(),
            Engine::new(state, state_tx, queue_tx),
        );
        let (head_tx, _) = watch::channel(head);
        let mut coordinator =
            SequencerEngineRequestCoordinator::new(processor, shadow, None, true, head_tx);
        let mut catchup = CanonicalUnsafeCatchup::default();
        for &(number, hash) in observations {
            catchup.buffer_payload(BaseExecutionPayloadEnvelope {
                execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                    block_number: number,
                    block_hash: B256::with_last_byte(hash),
                    parent_hash: B256::ZERO,
                    fee_recipient: Default::default(),
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: number,
                    extra_data: Default::default(),
                    base_fee_per_gas: Default::default(),
                    transactions: vec![],
                }),
                parent_beacon_block_root: None,
            });
        }
        catchup.commit(head);
        *coordinator.sequencer_state_mut() = SequencerEngineState::CatchingUp { shadow, catchup };

        let result = coordinator.prepare_sequencer_start(B256::with_last_byte(requested_hash));
        assert_eq!(result.is_ok(), accepted, "{result:?}");
        assert_eq!(
            matches!(coordinator.sequencer_state(), SequencerEngineState::Regular),
            accepted && !shadow
        );
        assert_eq!(
            matches!(
                coordinator.sequencer_state(),
                SequencerEngineState::CatchingUp { shadow: true, .. }
            ),
            shadow
        );
        // Acceptance must remain idempotent; refusal must not discard the blocking evidence.
        assert_eq!(
            coordinator.prepare_sequencer_start(B256::with_last_byte(requested_hash)).is_ok(),
            accepted
        );

        *coordinator.sequencer_state_mut() =
            SequencerEngineState::ShadowActive(Box::new(ShadowReconciliationGate::new(head)));
        assert_eq!(
            coordinator.prepare_sequencer_start(B256::with_last_byte(requested_hash)).is_ok(),
            requested_hash == 100
        );
        assert!(matches!(coordinator.sequencer_state(), SequencerEngineState::ShadowActive(_)));
    }

    #[rstest]
    #[case(B256::ZERO)]
    #[case(B256::with_last_byte(100))]
    fn prepare_start_rejects_uninitialized_head(
        #[case] requested_hash: B256,
        #[values(false, true)] shadow: bool,
    ) {
        let mut coordinator = coordinator(shadow, true, None);
        assert!(matches!(
            coordinator.prepare_sequencer_start(requested_hash),
            Err(crate::EngineClientError::RequestError(message)) if message.contains("no prestate")
        ));
    }
}
