//! Sequencer ownership and serialized routing of engine requests.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_consensus_engine::{
    ConsolidateTask, EngineClient, EngineSyncStateUpdate, EngineTask, EngineTaskError,
    EngineTaskErrorSeverity, EngineTaskErrors, FinalizeTask, Metrics as EngineMetrics,
    SealTaskError,
};
use opentelemetry::context::FutureExt as OtelFutureExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{self, Instant as TokioInstant, MissedTickBehavior},
};
use tracing::{debug, error, info, warn};

use super::{
    CanonicalUnsafeCatchup, Conductor, SequencerEngineState, SequencerKind,
    ShadowReconciliationGate,
};
use crate::{
    BuildRequest, EngineActorRequest, EngineClientError, EngineDerivationClient, EngineError,
    EngineProcessor, EngineRequestReceiver, GetPayloadRequest, InsertUnsafePayloadRequest, Metrics,
    NodeOperatingMode, ReconcileShadowRequest, ResetOrigin, ResetReason, ResetRequest,
    ResetRequestOutcome, actors::engine::ResetOutcome,
    HistoricalStartupFailure, HistoricalStartupHandoff,
};

const SEQUENCER_EL_SYNC_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const HISTORICAL_STARTUP_DEADLINE: Duration = Duration::from_secs(600);
const HISTORICAL_STARTUP_PROBE_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapRole {
    ActiveSequencer,
    ConductorFollower,
}

/// Records the outcome metric for one engine reset request and replies to its caller.
#[derive(Debug)]
pub struct ResetResponder {
    result_tx: mpsc::Sender<Result<(), EngineClientError>>,
    origin: ResetOrigin,
    reason: ResetReason,
    started: Instant,
    unsafe_before: base_protocol::L2BlockInfo,
}

impl ResetResponder {
    /// Starts timing `request`, whose unsafe head was `unsafe_before` when it arrived.
    pub fn new(request: ResetRequest, unsafe_before: base_protocol::L2BlockInfo) -> Self {
        Self {
            result_tx: request.result_tx,
            origin: request.origin,
            reason: request.reason,
            started: Instant::now(),
            unsafe_before,
        }
    }

    /// Replies without recording a reset metric. Returns whether the caller received the reply.
    pub async fn reply(&self, response: Result<(), EngineClientError>) -> bool {
        let delivered = self.result_tx.send(response).await.is_ok();
        if !delivered {
            warn!(target: "engine", origin = ?self.origin, "Sending reset response failed");
        }
        delivered
    }

    /// Records the reset `outcome` and replies. Returns whether the caller received the reply.
    pub async fn finish(
        &self,
        outcome: ResetRequestOutcome,
        unsafe_after: base_protocol::L2BlockInfo,
        response: Result<(), EngineClientError>,
    ) -> bool {
        Metrics::record_engine_reset(
            self.origin,
            self.reason,
            outcome,
            self.started.elapsed(),
            self.unsafe_before,
            unsafe_after,
        );
        self.reply(response).await
    }
}

/// Owns the engine processor and routes sequencer catch-up and shadow reconciliation requests.
#[derive(Debug)]
pub struct SequencerEngineRequestCoordinator<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    processor: EngineProcessor<EngineClient_, DerivationClient>,
    /// Canonical catch-up or active shadow reconciliation state.
    sequencer_state: SequencerEngineState,
    conductor: Option<Arc<dyn Conductor>>,
    sequencer_stopped: bool,
    sequencer_kind: SequencerKind,
    unsafe_head_tx: watch::Sender<base_protocol::L2BlockInfo>,
    /// Recent canonical payloads retained during catch-up, and the largest gap between the unsafe
    /// head and external gossip still admitted while regular. Both span the same wall-clock window.
    catchup_capacity: usize,
    /// Opt-in, caller-authenticated historical startup, completed before ordinary routing.
historical_startup: Option<Box<base_consensus_engine::SequencerStartupSync>>,
    /// Local head height admitted once, retained across gossip and label retries.
    historical_startup_initial_head: Option<u64>,
}

impl<EngineClient_, DerivationClient>
    SequencerEngineRequestCoordinator<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient,
    DerivationClient: EngineDerivationClient,
{
    /// Creates a request handler for the given operating mode.
    pub fn new(
        processor: EngineProcessor<EngineClient_, DerivationClient>,
        mode: NodeOperatingMode,
        conductor: Option<Arc<dyn Conductor>>,
        sequencer_stopped: bool,
        unsafe_head_tx: watch::Sender<base_protocol::L2BlockInfo>,
    ) -> Self {
        let catchup_capacity = CanonicalUnsafeCatchup::max_payloads_for(processor.rollup());
        Self {
            processor,
            sequencer_state: if mode.is_isolated() {
                SequencerEngineState::CatchingUp {
                    catchup: CanonicalUnsafeCatchup::new(catchup_capacity),
                }
            } else {
                SequencerEngineState::Regular
            },
            conductor,
            sequencer_stopped,
            sequencer_kind: mode.into(),
            unsafe_head_tx,
            catchup_capacity,
            historical_startup: None,
            historical_startup_initial_head: None,
        }
    }

    /// Opts a stopped regular coordinator into historical startup at an authenticated target.
    ///
    /// Call before `start`. The owner must supply a trusted target and no outstanding build jobs.
    /// This Rust API is intentionally not a new public RPC or a default change for existing nodes.
    pub fn begin_historical_startup(&mut self, target: base_protocol::L2BlockInfo) -> Result<(), EngineClientError> {
        if !self.sequencer_stopped || self.sequencer_kind != SequencerKind::Regular {
            return Err(EngineClientError::HistoricalStartupRefused("requires a stopped regular sequencer"));
        }
        self.historical_startup = Some(Box::new(base_consensus_engine::SequencerStartupSync::new(target)));
        self.historical_startup_initial_head = None;
        Ok(())
    }

    /// Refuses a reused target before the first FCU, without assuming local ancestry.
    ///
    /// Admission is shared by task entry and public probes. Once admitted, reaching or retargeting
    /// within the phase is expected and must not restart this cold-entry check.
    pub async fn admit_historical_startup_target(&mut self, remaining: Duration) -> Result<(), EngineError> {
        let target = self.historical_startup.as_ref().ok_or(HistoricalStartupFailure::NotSelected)?.target();
        if self.historical_startup_initial_head.is_some() {
            return Ok(());
        }
        if remaining.is_zero() {
            return Err(HistoricalStartupFailure::BudgetExhausted.into());
        }
        let local = time::timeout(remaining, self.processor.client().l2_block_info_by_label(BlockNumberOrTag::Latest)).await
            .map_err(|_| HistoricalStartupFailure::HeadQueryTimeout)?
            .map_err(|error| HistoricalStartupFailure::HeadQuery(Box::new(error)))?
            .ok_or(HistoricalStartupFailure::MissingHead)?;
        if local.block_info.number >= target.block_info.number {
            return Err(HistoricalStartupFailure::ReusedTarget { target: target.block_info.number, local: local.block_info.number }.into());
        }
        self.historical_startup_initial_head = Some(local.block_info.number);
        Ok(())
    }

    /// Confirms the selected target only after non-leader admission, within one shared budget.
    ///
    /// Admission failure revokes earlier readiness. The old engine completion latch is not used.
    pub async fn probe_historical_startup(&mut self, remaining: Duration) -> Result<base_consensus_engine::StartupSyncStatus, EngineError> {
        let started = TokioInstant::now();
        self.historical_startup.as_mut().ok_or(HistoricalStartupFailure::NotSelected)?.invalidate();
        if remaining.is_zero() {
            return Err(HistoricalStartupFailure::BudgetExhausted.into());
        }
        if let Some(conductor) = &self.conductor {
            let leader = time::timeout(remaining, conductor.leader()).await
                .map_err(|_| HistoricalStartupFailure::LeadershipTimeout)?
                .map_err(|error| HistoricalStartupFailure::Leadership(Box::new(error)))?;
            if leader {
                return Err(HistoricalStartupFailure::Leader.into());
            }
        }
        self.admit_historical_startup_target(remaining.saturating_sub(started.elapsed())).await?;
        let phase = self.historical_startup.as_mut().expect("phase admitted");
        phase.probe(self.processor.client().as_ref(), remaining.saturating_sub(started.elapsed())).await
            .map_err(|error| HistoricalStartupFailure::Probe(Box::new(error)).into())
    }

    /// Services startup requests without allowing ordinary engine queue work to execute.
    async fn handle_historical_startup_request(&mut self, request: EngineActorRequest) {
        match request {
            EngineActorRequest::PrepareSequencerStart { result_tx, .. } => {
                let _ = result_tx.try_send(Err(EngineClientError::HistoricalStartupPending));
            }
            EngineActorRequest::BuildRequest(request) => {
                let _ = request.result_tx.try_send(Err(base_consensus_engine::BuildTaskError::EngineBuildError(
                    base_consensus_engine::EngineBuildError::EngineSyncing,
                )));
            }
            EngineActorRequest::ResetRequest(request) => {
                // The stopped actor sends this reset on launch and recognizes ELSyncing as
                // deferred. A new error would terminate the actor before startup can finish.
                let error = match request.reason {
                    ResetReason::SequencerStartup => EngineClientError::ELSyncing,
                    _ => EngineClientError::HistoricalStartupPending,
                };
                let _ = request.result_tx.try_send(Err(error));
            }
            EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                // Only this request has already passed the network actor's signer authentication.
                let phase = self.historical_startup.as_mut().expect("startup request routing");
                if envelope.execution_payload.block_number() >= phase.target().block_info.number {
                    match base_protocol::L2BlockInfo::from_payload_and_genesis(
                        envelope.execution_payload, envelope.parent_beacon_block_root, &self.processor.rollup().genesis,
                    ) {
                        Ok(target) if target != phase.target() => phase.set_target(target),
                        Ok(_) => {}
                        Err(error) => warn!(target: "engine", %error, "Ignoring undecodable historical startup target"),
                    }
                }
            }
            // Derivation is not running and no local job may exist at cold-start entry. Dropping
            // other requests closes response channels without executing FCU or newPayload. Normal
            // derivation is reset at handoff, so stale unsolicited signals are not replayed.
            _ => {}
        }
    }

    /// Exclusively owns coordinator engine calls through target confirmation and handoff.
    async fn run_historical_startup(&mut self, requests: &mut mpsc::Receiver<EngineActorRequest>) -> Result<(), EngineError> {
        let deadline = TokioInstant::now() + HISTORICAL_STARTUP_DEADLINE;
        if let Some(conductor) = &self.conductor {
            let leader = time::timeout(HISTORICAL_STARTUP_PROBE_BUDGET, conductor.leader()).await
                .map_err(|_| HistoricalStartupFailure::LeadershipTimeout)?
                .map_err(|error| HistoricalStartupFailure::Leadership(Box::new(error)))?;
            if leader {
                return Err(HistoricalStartupFailure::Leader.into());
            }
        }
        let remaining = deadline.saturating_duration_since(TokioInstant::now()).min(HISTORICAL_STARTUP_PROBE_BUDGET);
        self.admit_historical_startup_target(remaining).await?;
        let mut interval = time::interval(SEQUENCER_EL_SYNC_PROBE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                request = requests.recv() => {
                    // Ordinary engine routing is inaccessible while this loop owns requests.
                    let request = request.ok_or(EngineError::ChannelClosed)?;
                    self.handle_historical_startup_request(request).await;
                }
                _ = time::sleep_until(deadline) => {
                    return Err(HistoricalStartupFailure::DeadlineExceeded.into());
                }
                _ = interval.tick() => {
                    let remaining = deadline.saturating_duration_since(TokioInstant::now()).min(HISTORICAL_STARTUP_PROBE_BUDGET);
                    if remaining.is_zero() {
                        return Err(HistoricalStartupFailure::DeadlineExceeded.into());
                    }
                    let status = self.probe_historical_startup(remaining).await?;
                    if status == base_consensus_engine::StartupSyncStatus::ExecutionReady {
                        let target = self.historical_startup.as_ref().expect("startup phase").target();
                        if let Some(conductor) = &self.conductor {
                            let remaining = deadline.saturating_duration_since(TokioInstant::now()).min(HISTORICAL_STARTUP_PROBE_BUDGET);
                            let leader = time::timeout(remaining, conductor.leader()).await
                                .map_err(|_| HistoricalStartupFailure::LeadershipTimeout)?
                                .map_err(|error| HistoricalStartupFailure::Leadership(Box::new(error)))?;
                            if leader {
                                return Err(HistoricalStartupFailure::Leader.into());
                            }
                        }
                        let outcome = time::timeout_at(deadline, self.processor.handoff_historical_startup(target)).await
                            .map_err(|_| HistoricalStartupFailure::HandoffTimeout)??;
                        if outcome == HistoricalStartupHandoff::Pending {
                            self.historical_startup.as_mut().expect("startup phase").invalidate();
                            continue;
                        }
                        // Normal routing starts only after canonical reset and completion delivery.
                        self.historical_startup = None;
                        self.complete_startup(target);
                        return Ok(());
                    }
                }
            }
        }
    }

    const fn is_isolated_active(&self) -> bool {
        matches!(self.sequencer_state, SequencerEngineState::IsolatedActive)
    }

    /// Returns the coordinator's sequencer routing state.
    pub const fn sequencer_state(&self) -> &SequencerEngineState {
        &self.sequencer_state
    }

    /// Returns mutable sequencer routing state for test harness setup.
    pub const fn sequencer_state_mut(&mut self) -> &mut SequencerEngineState {
        &mut self.sequencer_state
    }

    fn startup_complete(&self, catchup: &CanonicalUnsafeCatchup) -> bool {
        let engine = self.processor.engine_state();
        match self.sequencer_kind {
            SequencerKind::Regular | SequencerKind::Shadow => {
                catchup.is_complete(engine.sync_state.unsafe_head(), engine.sync_state.safe_head())
            }
            SequencerKind::Isolated => {
                // Equivocation leaves no single canonical parent; stall until reset/restart clears it.
                engine.el_sync_finished
                    && catchup.is_at_latest_observed(engine.sync_state.unsafe_head())
            }
        }
    }

    fn complete_startup(&mut self, head: base_protocol::L2BlockInfo) {
        if self.sequencer_kind == SequencerKind::Isolated {
            // Deferred canonical tasks must not survive onto the private chain.
            self.processor.engine_mut().clear();
        }
        self.sequencer_state = self.sequencer_kind.completed_state(head);
    }

    /// Validates an authorized admin start against the serialized engine state.
    ///
    /// Conductor supplies its committed head to admin start even when it has no new payload to
    /// post. That head can release a regular conductor follower's unobserved catch-up, but cannot
    /// override known gaps or conflicts. The caller must check conductor leadership before activation.
    /// Shadow starts validate the head without changing catch-up or reconciliation state.
    pub fn prepare_sequencer_start(
        &mut self,
        expected_hash: B256,
    ) -> Result<(), EngineClientError> {
        let sync_state = self.processor.engine_state().sync_state;
        if self.historical_startup.is_some() {
            return Err(EngineClientError::HistoricalStartupPending);
        }
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

        // Admin start must not release shadow catch-up or reconciliation.
        if self.is_shadow_sequencer() {
            return Ok(());
        }
        if self.sequencer_kind == SequencerKind::Regular
            && !self.processor.engine_state().el_sync_finished
        {
            return Err(EngineClientError::ELSyncing);
        }
        if let SequencerEngineState::CatchingUp { catchup } = &self.sequencer_state {
            if catchup.is_faulted() {
                return Err(EngineClientError::ShadowBufferFaulted);
            }
            let unobserved_follower =
                self.sequencer_kind == SequencerKind::Regular && !catchup.has_observations();
            if !unobserved_follower && !self.startup_complete(catchup) {
                return Err(EngineClientError::ELSyncing);
            }
        }

        if matches!(
            self.sequencer_state,
            SequencerEngineState::Regular | SequencerEngineState::CatchingUp { .. }
        ) {
            self.complete_startup(head);
        }
        Ok(())
    }

    const fn is_shadow_sequencer(&self) -> bool {
        matches!(self.sequencer_kind, SequencerKind::Shadow)
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
        if self.sequencer_stopped || self.sequencer_kind != SequencerKind::Regular {
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

    async fn handle_catching_up_reset(
        &mut self,
        origin: ResetOrigin,
        responder: &ResetResponder,
    ) -> Result<bool, EngineError> {
        let sync_state = self.processor.engine_state().sync_state;
        let head = sync_state.unsafe_head();

        let SequencerEngineState::CatchingUp { catchup } = &self.sequencer_state else {
            return Ok(false);
        };
        let kind = self.sequencer_kind;
        let faulted = catchup.is_faulted();
        let complete = self.startup_complete(catchup);

        match (kind, faulted, complete) {
            (_, true, _) => {
                error!(target: "engine", "Canonical catch-up payload buffer is faulted");
                responder
                    .finish(
                        ResetRequestOutcome::Failed,
                        head,
                        Err(EngineClientError::ShadowBufferFaulted),
                    )
                    .await;
            }
            (_, _, false) => {
                warn!(target: "engine", "Deferring sequencer reset until canonical startup completes");
                responder
                    .finish(ResetRequestOutcome::Deferred, head, Err(EngineClientError::ELSyncing))
                    .await;
            }
            (SequencerKind::Shadow, _, _) if origin != ResetOrigin::ShadowCycleCoordinated => {
                responder
                    .finish(
                        ResetRequestOutcome::Failed,
                        head,
                        Err(EngineClientError::ShadowReconciliationDisabled),
                    )
                    .await;
            }
            (SequencerKind::Shadow | SequencerKind::Isolated, _, _) => {
                info!(
                    target: "engine",
                    canonical_head = head.block_info.number,
                    canonical_hash = %head.block_info.hash,
                    "Private sequencer canonical startup completed"
                );
                self.complete_startup(head);
                self.unsafe_head_tx.send_replace(head);
                responder
                    .finish(ResetRequestOutcome::from_unsafe_heads(head, head), head, Ok(()))
                    .await;
            }
            (SequencerKind::Regular, _, _) => {
                info!(
                    target: "engine",
                    canonical_head = head.block_info.number,
                    canonical_hash = %head.block_info.hash,
                    "Sequencer canonical catch-up completed"
                );
                self.complete_startup(head);
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn handle_reset_request(
        &mut self,
        reset_request: ResetRequest,
    ) -> Result<(), EngineError> {
        let origin = reset_request.origin;
        let unsafe_before = self.processor.engine_state().sync_state.unsafe_head();
        let responder = ResetResponder::new(reset_request, unsafe_before);

        if origin == ResetOrigin::Derivation && self.is_isolated_active() {
            debug!(target: "engine", "Isolated sequencer ignoring derivation reset");
            responder.reply(Ok(())).await;
            return Ok(());
        }

        if matches!(origin, ResetOrigin::Sequencer | ResetOrigin::ShadowCycleCoordinated)
            && self.handle_catching_up_reset(origin, &responder).await?
        {
            return Ok(());
        }

        if !self.processor.engine_state().el_sync_finished {
            warn!(target: "engine", "Deferring engine reset: EL sync not yet complete");
            responder
                .finish(
                    ResetRequestOutcome::Deferred,
                    self.processor.engine_state().sync_state.unsafe_head(),
                    Err(EngineClientError::ELSyncing),
                )
                .await;
            return Ok(());
        }

        warn!(target: "engine", "Received reset request");

        let reset_res = self.processor.reset_engine_state().await;
        if let Ok(safe_head) = &reset_res {
            let anchor = self.processor.engine_state().sync_state.unsafe_head();
            match &mut self.sequencer_state {
                SequencerEngineState::ShadowActive(gate) => gate.reanchor(anchor),
                SequencerEngineState::CatchingUp { catchup } => {
                    *catchup = CanonicalUnsafeCatchup::new(self.catchup_capacity);
                }
                SequencerEngineState::Regular | SequencerEngineState::IsolatedActive => {}
            }
            self.unsafe_head_tx.send_replace(anchor);
            if let Err(error) = self.processor.notify_derivation_of_reset(*safe_head).await {
                responder
                    .finish(
                        ResetRequestOutcome::DerivationNotificationFailed,
                        self.processor.engine_state().sync_state.unsafe_head(),
                        Err(EngineClientError::ResetForkchoiceError(error.to_string())),
                    )
                    .await;
                if self.is_shadow_active() {
                    return Err(error);
                }
                return Ok(());
            }
        }

        let unsafe_after = self.processor.engine_state().sync_state.unsafe_head();
        let reset_outcome = if reset_res.is_ok() {
            ResetRequestOutcome::from_unsafe_heads(unsafe_before, unsafe_after)
        } else {
            ResetRequestOutcome::Failed
        };
        let response_payload = reset_res
            .as_ref()
            .map(|_| ())
            .map_err(|e| EngineClientError::ResetForkchoiceError(e.to_string()));
        let reset_succeeded = reset_res.is_ok();
        if !responder.finish(reset_outcome, unsafe_after, response_payload).await {
            reset_res?;
        }
        if reset_succeeded
            && self.is_shadow_active()
            && origin != ResetOrigin::ShadowCycleCoordinated
        {
            return Err(EngineError::ShadowInternalReset);
        }
        Ok(())
    }

    async fn probe_isolated_el_sync(&mut self) {
        let SequencerEngineState::CatchingUp { catchup } = &self.sequencer_state else {
            return;
        };
        if self.startup_complete(catchup) {
            return;
        }
        let Some(head) = catchup.latest_observed_head() else { return };
        let unsafe_head = self.processor.engine_state().sync_state.unsafe_head();
        let same_head = head.block_info.number == unsafe_head.block_info.number
            && head.block_info.hash == unsafe_head.block_info.hash;
        if !same_head && head.block_info.number <= unsafe_head.block_info.number {
            return;
        }

        let client = Arc::clone(self.processor.client());
        let rollup = Arc::clone(self.processor.rollup());
        let update = EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() };
        if let Err(err) = self.processor.engine_mut().probe_el_sync(client, rollup, update).await {
            warn!(target: "engine", error = %err, "Isolated canonical EL sync probe failed; will retry");
        }
    }

    /// Applies one bounded batch of the buffered canonical suffix.
    ///
    /// Returns whether more buffered payloads may remain, so the caller can serve queued requests
    /// between batches instead of blocking on one long run.
    async fn advance_canonical_catchup(&mut self) -> Result<bool, EngineError> {
        let anchor = self.processor.engine_state().sync_state.unsafe_head();
        let payloads = match &mut self.sequencer_state {
            SequencerEngineState::CatchingUp { catchup } => {
                catchup.contiguous_payloads(anchor, CanonicalUnsafeCatchup::MAX_APPLY_BATCH)
            }
            _ => return Ok(false),
        };
        if payloads.is_empty() {
            return Ok(false);
        }
        let more_pending = payloads.len() == CanonicalUnsafeCatchup::MAX_APPLY_BATCH;

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
                if let SequencerEngineState::CatchingUp { catchup } = &mut self.sequencer_state {
                    catchup.commit(head);
                }
                Ok(more_pending)
            }
            Err(error) => {
                debug!(
                    target: "engine",
                    error = ?error,
                    severity = ?error.severity(),
                    "Ordered canonical unsafe payload application deferred"
                );
                match error.severity() {
                    EngineTaskErrorSeverity::Temporary | EngineTaskErrorSeverity::Deferred => {
                        Ok(false)
                    }
                    _ => Err(EngineError::CriticalEngineTask(format!("{error:?}"))),
                }
            }
        }
    }
}

impl<EngineClient_, DerivationClient> EngineRequestReceiver
    for SequencerEngineRequestCoordinator<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    fn start(
        mut self,
        mut request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        tokio::spawn(async move {
            // Bootstrap: pre-populate the unsafe_head_tx watch channel so that external callers
            // An explicitly selected historical phase owns the connection before bootstrap.
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
            let historical_startup_selected = self.historical_startup.is_some();
            if historical_startup_selected {
                self.run_historical_startup(&mut request_channel).await?;
            }
            if !historical_startup_selected {
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
            if matches!(self.sequencer_state, SequencerEngineState::Regular) {
                self.sequencer_state = match (self.sequencer_kind, bootstrap_role, at_genesis) {
                    (SequencerKind::Shadow, _, _)
                    | (_, BootstrapRole::ConductorFollower, false) => {
                        SequencerEngineState::CatchingUp {
                            catchup: CanonicalUnsafeCatchup::new(self.catchup_capacity),
                        }
                    }
                    (kind, _, _) => {
                        kind.completed_state(self.processor.engine_state().sync_state.unsafe_head())
                    }
                };
            }
            if self.sequencer_kind == SequencerKind::Isolated {
                // Follow validator EL sync: a follower probe can declare sync at the local head
                // before canonical gossip arrives.
                self.processor.bootstrap_validator(opt_head).await;
            } else {
                match bootstrap_role {
                    BootstrapRole::ConductorFollower => {
                        self.processor.bootstrap_conductor_follower(opt_head).await
                    }
                    BootstrapRole::ActiveSequencer => {
                        self.processor.bootstrap_active_sequencer(opt_head, at_genesis).await
                    }
                }
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
                if drain_outcome == ResetOutcome::Reset {
                    if self.is_shadow_active() {
                        return Err(EngineError::ShadowInternalReset);
                    }
                    if let SequencerEngineState::CatchingUp { catchup } = &mut self.sequencer_state
                    {
                        *catchup = CanonicalUnsafeCatchup::new(self.catchup_capacity);
                    }
                }

                let more_catchup_pending = self.advance_canonical_catchup().await?;

                // If the unsafe head has updated, propagate it to the outbound channels.
                self.unsafe_head_tx.send_if_modified(|val| {
                    let new_head = self.processor.engine_state().sync_state.unsafe_head();
                    if *val == new_head {
                        return false;
                    }
                    *val = new_head;
                    true
                });

                // Wait for the next processing request.
                let recv_wait_timer = base_metrics::timed!(
                    EngineMetrics::engine_processor_recv_wait_duration_seconds()
                );
                let recv_result = if more_catchup_pending {
                    // Serve any queued request, then continue the next catch-up batch without
                    // waiting for new traffic.
                    match request_channel.try_recv() {
                        Ok(request) => Some(request),
                        Err(mpsc::error::TryRecvError::Empty) => {
                            drop(recv_wait_timer);
                            continue;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => None,
                    }
                } else if (self.processor.engine_state().el_sync_finished
                    && self.sequencer_kind != SequencerKind::Isolated)
                    || self.is_isolated_active()
                {
                    request_channel.recv().await
                } else {
                    tokio::select! {
                        request = request_channel.recv() => request,
                        _ = el_sync_probe_interval.tick() => {
                            drop(recv_wait_timer);
                            if self.sequencer_kind == SequencerKind::Isolated {
                                self.probe_isolated_el_sync().await;
                            } else {
                                let active_sequencer = self.resolve_bootstrap_role().await
                                    == BootstrapRole::ActiveSequencer;
                                self.processor
                                    .probe_sequencer_el_sync(active_sequencer)
                                    .await;
                            }
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
                    EngineActorRequest::ProcessDerivedAttributesRequest(_)
                    | EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(_)
                        if self.is_isolated_active() =>
                    {
                        debug!(target: "engine", "Isolated sequencer dropping canonical derived attributes or finalized block");
                    }
                    EngineActorRequest::ProcessDerivedAttributesRequest(attributes) => {
                        // Canonical ancestors of the cycle anchor cannot reorg the private unsafe
                        // branch, so confirm them immediately and let derivation keep advancing.
                        if let Some(gate) = self.active_shadow_gate()
                            && gate.should_defer_derived_attributes(&attributes)
                        {
                            gate.buffer_derived_attributes(attributes);
                            continue;
                        }
                        let task = EngineTask::Consolidate(Box::new(ConsolidateTask::new(
                            Arc::clone(self.processor.client()),
                            Arc::clone(self.processor.rollup()),
                            *attributes,
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
                        if let Some(gate) = self.active_shadow_gate()
                            && gate.should_defer_finalized(
                                *finalized_l2_block_number,
                                safe_head_number,
                            )
                        {
                            gate.buffer_finalized(*finalized_l2_block_number);
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
                            SequencerEngineState::CatchingUp { catchup } => {
                                match self.sequencer_kind {
                                    SequencerKind::Regular | SequencerKind::Shadow => {
                                        catchup.buffer_payload(*envelope)
                                    }
                                    SequencerKind::Isolated => {
                                        if let Err(err) = catchup.observe_isolated_payload(
                                            &envelope,
                                            &self.processor.rollup().genesis,
                                        ) {
                                            warn!(target: "engine", error = %err, "Failed to decode isolated canonical EL sync target");
                                        }
                                        self.processor.handle_external_unsafe_l2_block(*envelope);
                                    }
                                }
                            }
                            SequencerEngineState::ShadowActive(gate) => {
                                gate.buffer_payload(*envelope);
                            }
                            SequencerEngineState::IsolatedActive => {
                                debug!(target: "engine", "Isolated sequencer dropping canonical unsafe payload");
                            }
                            SequencerEngineState::Regular => {
                                let block_number = envelope.execution_payload.block_number();
                                let unsafe_head =
                                    self.processor.engine_state().sync_state.unsafe_head();
                                let max_external_unsafe_gap = self.catchup_capacity as u64;
                                let block_gap =
                                    block_number.checked_sub(unsafe_head.block_info.number);
                                if block_gap
                                    .is_some_and(|gap| gap > 0 && gap <= max_external_unsafe_gap)
                                {
                                    info!(
                                        target: "engine",
                                        block_number,
                                        block_hash = %envelope.execution_payload.block_hash(),
                                        parent_hash = %envelope.execution_payload.parent_hash(),
                                        block_gap = ?block_gap,
                                        max_external_unsafe_gap,
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
                                        max_external_unsafe_gap,
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
                            SequencerEngineState::CatchingUp { .. }
                                if self.is_shadow_sequencer() =>
                            {
                                warn!(target: "engine", "Ignoring admin unsafe payload during canonical catch-up");
                            }
                            SequencerEngineState::ShadowActive(_) => {
                                warn!(target: "engine", "Ignoring admin unsafe payload on shadow sequencer");
                            }
                            SequencerEngineState::IsolatedActive => {
                                warn!(target: "engine", "Ignoring admin unsafe payload on isolated sequencer");
                            }
                            SequencerEngineState::Regular
                            | SequencerEngineState::CatchingUp { .. } => {
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
                            | SequencerEngineState::CatchingUp { .. }
                            | SequencerEngineState::IsolatedActive => {
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
                        self.handle_reset_request(*reset_request).await?;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
    };
    use base_common_consensus::BaseTxEnvelope;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::{
        ConsolidateTask, Engine, EngineState, EngineTask,
        test_utils::{
            MockEngineClient, TestAttributesBuilder, TestEngineStateBuilder,
            encoded_l1_info_deposit_tx, test_block_info, test_engine_client_builder,
        },
    };
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
    use jsonrpsee::core::ClientError;
    use rstest::rstest;
    use tokio::sync::{mpsc, watch};

    use super::{BootstrapRole, SequencerEngineRequestCoordinator, SequencerEngineState};
    use crate::{
        CanonicalUnsafeCatchup, Conductor, ConductorError, EngineActorRequest, EngineClientError,
        EngineProcessor, EngineRequestReceiver, MockConductor, MockEngineDerivationClient,
        NodeOperatingMode, ResetOrigin, ResetReason, ResetRequest, ShadowReconciliationGate,
    };

    fn coordinator(
        shadow: bool,
        stopped: bool,
        conductor: Option<Arc<dyn Conductor>>,
    ) -> SequencerEngineRequestCoordinator<MockEngineClient, MockEngineDerivationClient> {
        let mode = if shadow {
            NodeOperatingMode::ShadowSequencer { blocks_per_cycle: std::num::NonZeroU64::MIN }
        } else {
            NodeOperatingMode::Sequencer
        };
        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);
        let processor = EngineProcessor::new(
            Arc::new(test_engine_client_builder().with_block_info_by_tag(BlockNumberOrTag::Latest, test_block_info(0)).build()),
            Arc::new(RollupConfig::default()),
            MockEngineDerivationClient::new(),
            engine,
        );
        let (unsafe_head_tx, _) = watch::channel(L2BlockInfo::default());
        SequencerEngineRequestCoordinator::new(processor, mode, conductor, stopped, unsafe_head_tx)
    }

    const REGULAR_MODE: bool = false;
    const STOPPED: bool = true;

    /// Missing phase selection and missing execution heads fail without a panic or an FCU.
    #[tokio::test]
    async fn historical_startup_missing_selection_and_head_fail_closed() {
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, None);
        assert!(matches!(coordinator.probe_historical_startup(Duration::from_secs(1)).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::NotSelected))), "an unselected public probe must return a classified error");
        coordinator.begin_historical_startup(test_block_info(100)).expect("stopped target must be admitted");
        coordinator.processor.client().storage().write().await.block_info_by_tag.clear();
        let (_tx, mut rx) = mpsc::channel(1);
        assert!(matches!(coordinator.run_historical_startup(&mut rx).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::MissingHead))), "missing head must fail closed at admission");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "missing head must not allow FCU");
    }

    /// Full rejection channels must not park the phase inside a request branch.
    #[tokio::test]
    async fn historical_startup_full_reply_channels_do_not_block() {
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, None);
        let target = test_block_info(100);
        coordinator.begin_historical_startup(target).expect("stopped target must be admitted");
        let (result_tx, _rx) = mpsc::channel(1);
        result_tx.try_send(Err(EngineClientError::HistoricalStartupPending)).expect("fill activation response channel");
        tokio::time::timeout(Duration::from_millis(50), coordinator.handle_historical_startup_request(EngineActorRequest::PrepareSequencerStart { expected_hash: target.block_info.hash, result_tx })).await.expect("activation rejection must not wait for channel capacity");
        let (result_tx, _rx) = mpsc::channel(1);
        result_tx.try_send(Err(base_consensus_engine::BuildTaskError::EngineBuildError(base_consensus_engine::EngineBuildError::EngineSyncing))).expect("fill build response channel");
        tokio::time::timeout(Duration::from_millis(50), coordinator.handle_historical_startup_request(EngineActorRequest::BuildRequest(Box::new(crate::BuildRequest { attributes: TestAttributesBuilder::new().with_parent(target).build(), result_tx, otel_cx: Default::default() })))).await.expect("build rejection must not wait for channel capacity");
        let (result_tx, _rx) = mpsc::channel(1);
        result_tx.try_send(Err(EngineClientError::ELSyncing)).expect("fill reset response channel");
        tokio::time::timeout(Duration::from_millis(50), coordinator.handle_historical_startup_request(EngineActorRequest::ResetRequest(Box::new(ResetRequest { result_tx, origin: ResetOrigin::Sequencer, reason: ResetReason::SequencerStartup })))).await.expect("reset rejection must not wait for channel capacity");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "rejecting full reply channels must not reach the engine");
    }

    /// A leadership change after the last target VALID must be refused before label handoff.
    #[tokio::test]
    async fn historical_startup_rechecks_leadership_before_handoff() {
        let mut conductor = MockConductor::new();
        let mut sequence = mockall::Sequence::new();
        conductor.expect_leader().times(1).in_sequence(&mut sequence).returning(|| Ok(false));
        conductor.expect_leader().times(1).in_sequence(&mut sequence).returning(|| Ok(false));
        conductor.expect_leader().times(1).in_sequence(&mut sequence).returning(|| Ok(true));
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, Some(Arc::new(conductor)));
        coordinator.begin_historical_startup(test_block_info(100)).expect("stopped target must be admitted");
        coordinator.processor.client().set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None }).await;
        let (_tx, mut rx) = mpsc::channel(1);
        assert!(matches!(coordinator.run_historical_startup(&mut rx).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::Leader))), "handoff must refuse newly observed leadership");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 1, "only the target probe may run before handoff refusal");
        assert!(coordinator.historical_startup.is_some(), "leadership refusal must leave activation gated");
    }

    /// A label SYNCING retry never restarts the overall phase clock.
    #[tokio::test(start_paused = true)]
    async fn historical_startup_label_retry_respects_original_deadline() {
        let attributes = TestAttributesBuilder::new().with_parent(test_block_info(999)).with_transactions(vec![encoded_l1_info_deposit_tx(L1BlockInfoBedrock::default())]).build();
        let block = base_consensus_engine::test_utils::matching_rpc_block(&attributes);
        let target = L2BlockInfo::from_block_and_genesis(&block.clone().into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()), &RollupConfig::default().genesis).expect("fixture must contain a deposit");
        let client = Arc::new(test_engine_client_builder().with_block_info_by_tag(BlockNumberOrTag::Latest, target)
            .with_l2_block(BlockNumberOrTag::Latest.into(), block.clone()).with_l2_block(BlockNumberOrTag::Safe.into(), block.clone()).with_l2_block(BlockNumberOrTag::Finalized.into(), block).with_l1_block(0u64.into(), Default::default())
            .with_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).build());
        client.storage().write().await.latest_block_info_responses.push_back(Some(test_block_info(0)));
        client.storage().write().await.fork_choice_updated_v3_responses.push_back(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None });
        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0);
        let processor = EngineProcessor::new(Arc::clone(&client), Arc::new(RollupConfig::default()), MockEngineDerivationClient::new(), Engine::new(EngineState::default(), state_tx, queue_tx));
        let (head_tx, _) = watch::channel(L2BlockInfo::default());
        let mut coordinator = SequencerEngineRequestCoordinator::new(processor, NodeOperatingMode::Sequencer, None, STOPPED, head_tx);
        coordinator.begin_historical_startup(target).expect("stopped target must be admitted");
        let (_tx, mut rx) = mpsc::channel(1);
        let started = tokio::time::Instant::now();
        assert!(matches!(coordinator.run_historical_startup(&mut rx).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::DeadlineExceeded))), "pending label followed by continued syncing must end at the original deadline");
        assert_eq!(started.elapsed(), super::HISTORICAL_STARTUP_DEADLINE, "SYNCING retries must not renew the phase budget");
        assert!(coordinator.historical_startup.is_some(), "deadline expiry must retain the activation gate");
        assert!(!coordinator.processor.engine_state().el_sync_finished, "pending labels must not commit readiness");
    }

    /// Reusing a one-shot target must fail before FCU when local execution has reached or passed it.
    #[rstest]
    #[case::at_target(100, None)]
    #[case::same_height_conflict(100, Some(B256::repeat_byte(0xaa)))]
    #[case::past_target(101, None)]
    #[tokio::test]
    async fn historical_startup_refuses_reused_target_before_fcu(#[case] local_height: u64, #[case] hash_override: Option<B256>) {
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, None);
        let target = test_block_info(100);
        coordinator.begin_historical_startup(target).expect("stopped target must be admitted");
        let mut local = test_block_info(local_height);
        if let Some(hash) = hash_override { local.block_info.hash = hash; }
        coordinator.processor.client().storage().write().await.block_info_by_tag.insert(BlockNumberOrTag::Latest, local);
        let (_tx, mut rx) = mpsc::channel(1);
        assert!(matches!(coordinator.run_historical_startup(&mut rx).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::ReusedTarget { target: 100, local })) if local == local_height), "reused targets must return the specific removal-required error");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "restart must not send the stale target to the engine");
        assert!(matches!(coordinator.probe_historical_startup(Duration::from_secs(1)).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::ReusedTarget { .. }))), "a public pre-start probe must enforce the same one-shot admission");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "public probing must not bypass the stale-target guard");
    }

    /// Authenticated gossip beyond the active window replaces the target and revokes readiness.
    #[tokio::test]
    async fn historical_startup_gossip_retarget_revokes_readiness() {
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, None);
        let first = test_block_info(100);
        let client = Arc::clone(coordinator.processor.client());
        coordinator.begin_historical_startup(first).expect("stopped target must be admitted");
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None }).await;
        coordinator.probe_historical_startup(Duration::from_secs(1)).await.expect("first target must confirm");
        let envelope = payload(first.block_info.number + coordinator.catchup_capacity as u64 + 1, 42);
        let next = L2BlockInfo::from_payload_and_genesis(envelope.execution_payload.clone(), envelope.parent_beacon_block_root, &coordinator.processor.rollup().genesis).expect("gossip fixture must contain a deposit");
        coordinator.handle_historical_startup_request(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(envelope))).await;
        let phase = coordinator.historical_startup.as_ref().expect("phase must remain selected");
        assert_eq!(phase.target(), next, "gossip beyond the active gap limit must replace the target");
        assert_eq!(phase.status(), base_consensus_engine::StartupSyncStatus::Pending, "retargeting must revoke the earlier VALID immediately");
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).await;
        client.storage().write().await.block_info_by_tag.insert(BlockNumberOrTag::Latest, next);
        assert_eq!(coordinator.probe_historical_startup(Duration::from_secs(1)).await.expect("SYNCING is recoverable"), base_consensus_engine::StartupSyncStatus::Pending, "new target must require its own confirmation");
        let requests = client.storage().read().await.fork_choice_updated_v3_requests.clone();
        assert_eq!(requests.len(), 2, "each target must have exactly one confirmation attempt");
        assert_eq!(requests[1].0.head_block_hash, next.block_info.hash, "the following FCU must use the gossip target");
        assert!(!requests[1].1, "historical target confirmation must not build");
        assert!(client.last_new_payload_v2().await.is_none(), "retargeting must never insert the full gossip payload");
        coordinator.handle_historical_startup_request(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(payload(first.block_info.number - 1, 43)))).await;
        assert_eq!(coordinator.historical_startup.as_ref().unwrap().target(), next, "below-target gossip must not retarget");
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None }).await;
        coordinator.probe_historical_startup(Duration::from_secs(1)).await.expect("new target must confirm");
        let mut malformed = payload(next.block_info.number + 1, 44);
        let BaseExecutionPayload::V1(body) = &mut malformed.execution_payload else { panic!("fixture must be V1") };
        body.transactions.clear();
        for envelope in [payload(first.block_info.number - 1, 43), payload(next.block_info.number, 42), malformed] {
            coordinator.handle_historical_startup_request(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(envelope))).await;
            let phase = coordinator.historical_startup.as_ref().unwrap();
            assert_eq!(phase.target(), next, "old, duplicate or undecodable gossip must not replace the target");
            assert_eq!(phase.status(), base_consensus_engine::StartupSyncStatus::ExecutionReady, "ignored gossip must retain the current target confirmation");
        }
    }

    /// Entry admission and the first probe independently consult leadership before any engine call.
    #[tokio::test]
    async fn historical_startup_entry_admission_precedes_probe_admission() {
        let mut conductor = MockConductor::new();
        let mut sequence = mockall::Sequence::new();
        conductor.expect_leader().times(1).in_sequence(&mut sequence).returning(|| Ok(false));
        conductor.expect_leader().times(1).in_sequence(&mut sequence).returning(|| Ok(true));
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, Some(Arc::new(conductor)));
        coordinator.begin_historical_startup(test_block_info(100)).expect("stopped target must be admitted");
        let (_tx, mut rx) = mpsc::channel(1);
        assert!(coordinator.run_historical_startup(&mut rx).await.is_err(), "leadership acquired after entry must refuse the first probe");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "both entry and probe checks must precede engine calls");
    }

    /// A label SYNCING response leaves engine state uncommitted instead of killing startup.
    #[tokio::test]
    async fn historical_startup_label_syncing_defers_handoff() {
        let attributes = TestAttributesBuilder::new().with_parent(test_block_info(999)).with_transactions(vec![encoded_l1_info_deposit_tx(L1BlockInfoBedrock::default())]).build();
        let block = base_consensus_engine::test_utils::matching_rpc_block(&attributes);
        let target = L2BlockInfo::from_block_and_genesis(&block.clone().into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()), &RollupConfig::default().genesis).expect("fixture must contain an L1 info deposit");
        let mut coordinator = coordinator(REGULAR_MODE, STOPPED, None);
        let client = Arc::new(test_engine_client_builder().with_block_info_by_tag(BlockNumberOrTag::Latest, target)
            .with_l2_block(BlockNumberOrTag::Latest.into(), block.clone())
            .with_l2_block(BlockNumberOrTag::Safe.into(), block.clone())
            .with_l2_block(BlockNumberOrTag::Finalized.into(), block)
            .with_l1_block(0u64.into(), Default::default()).build());
        let (state_tx, _) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0);
        coordinator.processor = EngineProcessor::new(Arc::clone(&client), Arc::new(RollupConfig::default()), MockEngineDerivationClient::new(), Engine::new(EngineState::default(), state_tx, queue_tx));
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).await;
        let before = *coordinator.processor.engine_state();
        assert_eq!(coordinator.processor.handoff_historical_startup(target).await.expect("SYNCING is recoverable"), crate::HistoricalStartupHandoff::Pending, "label SYNCING must defer handoff rather than terminate startup");
        assert_eq!(*coordinator.processor.engine_state(), before, "pending labels must not commit readiness or engine labels");
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Invalid { validation_error: "invalid labels".into() }, latest_valid_hash: None }, payload_id: None }).await;
        assert!(matches!(coordinator.processor.handoff_historical_startup(target).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::LabelStatus(PayloadStatusEnum::Invalid { .. })))), "invalid labels must remain a typed terminal error");
        client.storage().write().await.fork_choice_updated_v3_response = None;
        assert!(matches!(coordinator.processor.handoff_historical_startup(target).await, Err(crate::EngineError::HistoricalStartup(crate::HistoricalStartupFailure::LabelRequest(_)))), "label transport errors must retain their source and stay terminal");
        assert_eq!(client.storage().read().await.fork_choice_updated_v3_requests.len(), 3, "each explicit handoff must issue only one label request, without internal retries");
        assert_eq!(*coordinator.processor.engine_state(), before, "terminal label failures must not commit state");
    }

    /// Historical startup must gate activation even if the old EL completion latch is true.
    #[tokio::test]
    async fn historical_startup_gate_is_independent_of_engine_latch() {
        let mut coordinator = coordinator(false, true, None);
        let target = test_block_info(100);
        coordinator.begin_historical_startup(target).expect("stopped regular sequencer accepts a startup target");
        assert!(matches!(coordinator.prepare_sequencer_start(target.block_info.hash), Err(EngineClientError::HistoricalStartupPending)), "activation must reject a pending target");
        assert!(coordinator.probe_historical_startup(Duration::ZERO).await.is_err(), "an exhausted budget must not confirm startup");
        assert!(matches!(coordinator.prepare_sequencer_start(target.block_info.hash), Err(EngineClientError::HistoricalStartupPending)), "failed probe must leave activation gated");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "no FCU may escape an expired budget");
    }

    /// The startup request router must reject mutations without reaching the execution client.
    #[tokio::test]
    async fn historical_startup_router_serializes_engine_ownership() {
        let mut coordinator = coordinator(false, true, None);
        let target = test_block_info(100);
        coordinator.begin_historical_startup(target).expect("startup target");
        let (result_tx, mut result_rx) = mpsc::channel(1);
        coordinator.handle_historical_startup_request(EngineActorRequest::BuildRequest(Box::new(crate::BuildRequest {
            attributes: TestAttributesBuilder::new().with_parent(target).build(),
            result_tx, otel_cx: Default::default(),
        }))).await;
        assert!(matches!(result_rx.recv().await.unwrap(), Err(base_consensus_engine::BuildTaskError::EngineBuildError(base_consensus_engine::EngineBuildError::EngineSyncing))), "building must return the named syncing error");
        coordinator.handle_historical_startup_request(EngineActorRequest::ProcessAdminUnsafeL2BlockRequest(Box::new(payload(101, 9)))).await;
        coordinator.handle_historical_startup_request(EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(Box::new(50))).await;
        assert!(coordinator.processor.client().last_new_payload_v2().await.is_none(), "admin inserts must not interleave with startup FCU");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "build and finalization must not send an FCU");
    }

    /// A previously latched engine still waits for this target and completes delivery before release.
    #[rstest]
    #[case::immediate(vec![PayloadStatusEnum::Valid, PayloadStatusEnum::Valid])]
    #[case::label_syncing(vec![PayloadStatusEnum::Valid, PayloadStatusEnum::Syncing, PayloadStatusEnum::Valid, PayloadStatusEnum::Valid])]
    #[tokio::test(start_paused = true)]
    async fn historical_startup_valid_handoff_releases_gate_after_derivation_delivery(#[case] statuses: Vec<PayloadStatusEnum>) {
        let old = test_block_info(100);
        let attributes = TestAttributesBuilder::new().with_parent(test_block_info(999))
            .with_transactions(vec![encoded_l1_info_deposit_tx(L1BlockInfoBedrock::default())]).build();
        let block = base_consensus_engine::test_utils::matching_rpc_block(&attributes);
        let target = L2BlockInfo::from_block_and_genesis(
            &block.clone().into_consensus()
                .map_transactions(|tx| tx.inner.inner.into_inner()), &RollupConfig::default().genesis,
        ).expect("fixture contains an L1 info deposit");
        let safe = target;
        let state = TestEngineStateBuilder::new().with_unsafe_head(old).with_safe_head(safe)
            .with_finalized_head(test_block_info(80)).with_el_sync_finished(true).build();
        let client = Arc::new(test_engine_client_builder()
            .with_block_info_by_tag(BlockNumberOrTag::Latest, target)
            .with_l2_block(BlockNumberOrTag::Latest.into(), block.clone())
            .with_l2_block(BlockNumberOrTag::Safe.into(), block.clone())
            .with_l2_block(BlockNumberOrTag::Finalized.into(), block)
            .with_l1_block(0u64.into(), Default::default())
            .with_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None }).build());
        let mut derivation = MockEngineDerivationClient::new();
        derivation.expect_send_signal().times(1).returning(|_| Ok(()));
        derivation.expect_send_new_engine_safe_head().times(1).withf(move |actual| *actual == safe).returning(|_| Ok(()));
        derivation.expect_notify_sync_completed().times(1).withf(move |actual| *actual == safe).returning(|_| Ok(()));
        let (state_tx, _) = watch::channel(state);
        let (queue_tx, _) = watch::channel(0);
        // Real sync-start selection must retain the safe/finalized labels supplied by this EL.
        let processor = EngineProcessor::new(Arc::clone(&client), Arc::new(RollupConfig::default()), derivation, Engine::new(state, state_tx, queue_tx));
        let (head_tx, _) = watch::channel(old);
        let mut coordinator = SequencerEngineRequestCoordinator::new(processor, NodeOperatingMode::Sequencer, None, true, head_tx);
        coordinator.begin_historical_startup(target).expect("startup target");
        assert!(matches!(coordinator.prepare_sequencer_start(old.block_info.hash), Err(EngineClientError::HistoricalStartupPending)), "old VALID latch must not authorize activation");
        let (_tx, mut rx) = mpsc::channel(4);
        let expected_calls = statuses.len();
        client.storage().write().await.fork_choice_updated_v3_responses.extend(statuses.into_iter().map(|status| ForkchoiceUpdated { payload_status: PayloadStatus { status, latest_valid_hash: None }, payload_id: None }));
        client.storage().write().await.latest_block_info_responses.push_back(Some(old));
        coordinator.run_historical_startup(&mut rx).await.expect("VALID and derivation delivery complete handoff");
        assert!(coordinator.historical_startup.is_none(), "the gate releases only after successful delivery");
        assert_eq!(coordinator.processor.engine_state().sync_state.unsafe_head(), target, "normal routing must inherit the confirmed target");
        assert!(coordinator.prepare_sequencer_start(target.block_info.hash).is_ok(), "activation may proceed at the handed-off target");
        assert_eq!(client.storage().read().await.fork_choice_updated_v3_requests.len(), expected_calls, "each label SYNCING must require another target probe and label confirmation before delivery");
    }

    /// Confirmation and retargeting never consult or modify the old readiness latch.
    #[tokio::test]
    async fn historical_startup_retarget_syncing_and_error_remain_gated() {
        let mut coordinator = coordinator(false, true, None);
        let first = test_block_info(100);
        let second = test_block_info(200);
        let client = Arc::clone(coordinator.processor.client());
        coordinator.begin_historical_startup(first).expect("initial target");
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None }).await;
        assert_eq!(coordinator.probe_historical_startup(Duration::from_secs(1)).await.unwrap(), base_consensus_engine::StartupSyncStatus::ExecutionReady, "VALID confirms execution but not handoff");
        assert!(matches!(coordinator.prepare_sequencer_start(first.block_info.hash), Err(EngineClientError::HistoricalStartupPending)), "raw VALID alone must not authorize activation");
        coordinator.begin_historical_startup(second).expect("replacement target");
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).await;
        assert_eq!(coordinator.probe_historical_startup(Duration::from_secs(1)).await.unwrap(), base_consensus_engine::StartupSyncStatus::Pending, "new target SYNCING must be pending");
        client.storage().write().await.fork_choice_updated_v3_response = None;
        assert!(coordinator.probe_historical_startup(Duration::from_secs(1)).await.is_err(), "RPC failure must fail closed");
        assert!(matches!(coordinator.prepare_sequencer_start(second.block_info.hash), Err(EngineClientError::HistoricalStartupPending)), "retarget and RPC failure cannot reuse earlier VALID");
        assert!(!coordinator.processor.engine_state().el_sync_finished, "raw probes must not set the old latch");
        let requests = client.storage().read().await.fork_choice_updated_v3_requests.clone();
        assert_eq!(requests.len(), 3, "each explicit probe must reach EL exactly once");
        assert_eq!(requests[0].0.head_block_hash, first.block_info.hash, "first probe target");
        assert_eq!(requests[1].0.head_block_hash, second.block_info.hash, "retargeted SYNCING probe");
        assert_eq!(requests[2].0.head_block_hash, second.block_info.hash, "failed reconfirmation target");
    }

    /// The real coordinator task enters startup before ordinary bootstrap and engine draining.
    #[tokio::test]
    async fn historical_startup_task_rejects_build_and_admin_start() {
        let mut coordinator = coordinator(false, true, None);
        let target = test_block_info(1000);
        let client = Arc::clone(coordinator.processor.client());
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).await;
        coordinator.begin_historical_startup(target).expect("inactive startup target");
        let (tx, rx) = mpsc::channel(8);
        let task = coordinator.start(rx);
        let (result_tx, mut result_rx) = mpsc::channel(1);
        tx.send(EngineActorRequest::BuildRequest(Box::new(crate::BuildRequest { attributes: TestAttributesBuilder::new().with_parent(target).build(), result_tx, otel_cx: Default::default() }))).await.expect("build request");
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), result_rx.recv()).await.unwrap().unwrap(), Err(base_consensus_engine::BuildTaskError::EngineBuildError(base_consensus_engine::EngineBuildError::EngineSyncing))), "real task must reject a build while startup owns EL");
        tx.send(EngineActorRequest::ProcessAdminUnsafeL2BlockRequest(Box::new(payload(1001, 8)))).await.expect("admin payload");
        let (result_tx, mut result_rx) = mpsc::channel(1);
        tx.send(EngineActorRequest::PrepareSequencerStart { expected_hash: target.block_info.hash, result_tx }).await.expect("activation request");
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), result_rx.recv()).await.unwrap().unwrap(), Err(EngineClientError::HistoricalStartupPending)), "real activation path must stay gated");
        assert!(client.last_new_payload_v2().await.is_none(), "admin newPayload must not interleave");
        assert!(client.storage().read().await.fork_choice_updated_v3_requests.iter().all(|(fc, attributes)| fc.head_block_hash == target.block_info.hash && !attributes), "every engine call during startup must be a target-only FCU");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled(), "the test must stop its coordinator task");
    }

    /// The stopped actor must defer its automatic initial reset rather than terminate startup.
    #[tokio::test]
    async fn historical_startup_keeps_stopped_sequencer_actor_alive() {
        let mut coordinator = coordinator(false, true, None);
        let target = test_block_info(1000);
        let client = Arc::clone(coordinator.processor.client());
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).await;
        coordinator.begin_historical_startup(target).expect("historical target");
        let (request_tx, request_rx) = mpsc::channel(8);
        let (_head_tx, head_rx) = watch::channel(target);
        let (_state_tx, state_rx) = watch::channel(*coordinator.processor.engine_state());
        let engine_client = Arc::new(crate::QueuedSequencerEngineClient::new(request_tx.clone(), head_rx, state_rx));
        let mut seed = super::super::tests::test_actor();
        Arc::make_mut(&mut seed.rollup_config).block_time = 2;
        seed.builder.rollup_config = Arc::clone(&seed.rollup_config);
        let actor = crate::SequencerActor {
            builder: crate::PayloadBuilder {
                attributes_builder: seed.builder.attributes_builder,
                origin_selector: seed.builder.origin_selector,
                recovery_mode: seed.builder.recovery_mode,
                rollup_config: seed.builder.rollup_config,
                engine_client: Arc::clone(&engine_client),
            },
            admin_api_rx: seed.admin_api_rx, cancellation_token: seed.cancellation_token,
            conductor: seed.conductor, engine_client, is_active: false, mode: seed.mode,
            shadow_funding: seed.shadow_funding, recovery_mode: seed.recovery_mode,
            rollup_config: seed.rollup_config, seal_offset: seed.seal_offset,
            schedule_delay: seed.schedule_delay, unsafe_payload_gossip_client: seed.unsafe_payload_gossip_client,
            sealer: seed.sealer, pending_stop: seed.pending_stop, payload_ready: seed.payload_ready,
        };
        let cancellation = actor.cancellation_token.clone();
        let coordinator_task = coordinator.start(request_rx);
        let mut actor_task = tokio::spawn(crate::NodeActor::start(actor, ()));
        tokio::select! {
            result = &mut actor_task => {
                coordinator_task.abort();
                panic!("stopped sequencer must wait through historical startup, got {result:?}");
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        cancellation.cancel();
        actor_task.await.expect("actor task must join").expect("deferred initial reset must honor cancellation");
        assert!(client.last_new_payload_v2().await.is_none(), "the stopped actor cannot insert a payload");
        coordinator_task.abort();
        assert!(coordinator_task.await.unwrap_err().is_cancelled(), "the test must stop its coordinator");
        drop(request_tx);
    }

    /// Public pre-start probing must not bypass conductor admission enforced by task entry.
    #[tokio::test]
    async fn historical_startup_public_probe_rejects_leader_before_engine_calls() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().times(1).returning(|| Ok(true));
        let mut coordinator = coordinator(false, true, Some(Arc::new(conductor)));
        coordinator.begin_historical_startup(test_block_info(100)).expect("stopped target");
        coordinator.processor.client().set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None }, payload_id: None }).await;
        assert!(coordinator.probe_historical_startup(Duration::from_secs(1)).await.is_err(), "public probes must refuse a configured leader before sending FCU");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "no public pre-start FCU may bypass leadership admission");
    }

    /// A configured leader cannot enter the inactive phase, even when locally stopped.
    #[tokio::test]
    async fn historical_startup_rejects_conductor_leader_before_engine_calls() {
        let mut conductor = MockConductor::new();
        conductor.expect_leader().times(1).returning(|| Ok(true));
        let mut coordinator = coordinator(false, true, Some(Arc::new(conductor)));
        coordinator.begin_historical_startup(test_block_info(100)).expect("stopped target");
        let (_tx, mut rx) = mpsc::channel(1);
        assert!(coordinator.run_historical_startup(&mut rx).await.is_err(), "a leader must use a separate initialization path");
        assert!(coordinator.historical_startup.is_some(), "failed entry must not release the gate");
        assert_eq!(coordinator.processor.client().storage().read().await.fork_choice_updated_v3_requests.len(), 0, "leadership admission runs before any engine call");
    }

    /// Active or private sequencing must not enter a canonical historical startup phase.
    #[test]
    fn historical_startup_requires_stopped_regular_sequencer() {
        let mut active = coordinator(false, false, None);
        assert!(active.begin_historical_startup(test_block_info(100)).is_err(), "an active sequencer cannot enter startup");
        let mut shadow = coordinator(true, true, None);
        assert!(shadow.begin_historical_startup(test_block_info(100)).is_err(), "a shadow sequencer cannot enter regular startup");
    }

    fn payload(number: u64, hash: u8) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
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
                transactions: vec![encoded_l1_info_deposit_tx(L1BlockInfoBedrock::default())],
            }),
            parent_beacon_block_root: None,
        }
    }

    fn isolated_startup_coordinator()
    -> SequencerEngineRequestCoordinator<MockEngineClient, MockEngineDerivationClient> {
        let head = test_block_info(100);
        let state = TestEngineStateBuilder::new()
            .with_finalized_head(test_block_info(0))
            .with_el_sync_finished(false)
            .build();
        let (state_tx, _) = watch::channel(state);
        let (queue_tx, _) = watch::channel(0usize);
        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, head)
                .with_fork_choice_updated_v3_response(ForkchoiceUpdated {
                    payload_status: PayloadStatus {
                        status: PayloadStatusEnum::Syncing,
                        latest_valid_hash: None,
                    },
                    payload_id: None,
                })
                .build(),
        );
        let mut derivation = MockEngineDerivationClient::new();
        derivation.expect_notify_sync_completed().returning(|_| Ok(()));
        let processor = EngineProcessor::new(
            client,
            Arc::new(RollupConfig::default()),
            derivation,
            Engine::new(state, state_tx, queue_tx),
        );
        let (head_tx, _) = watch::channel(head);
        SequencerEngineRequestCoordinator::new(
            processor,
            NodeOperatingMode::IsolatedSequencer,
            None,
            true,
            head_tx,
        )
    }

    async fn request_start(
        request_tx: &mpsc::Sender<EngineActorRequest>,
    ) -> Result<(), EngineClientError> {
        let (result_tx, mut result_rx) = mpsc::channel(1);
        request_tx
            .send(EngineActorRequest::ResetRequest(Box::new(ResetRequest {
                result_tx,
                origin: ResetOrigin::Sequencer,
                reason: ResetReason::SequencerStartup,
            })))
            .await
            .expect("startup request");
        tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
            .await
            .expect("startup timed out")
            .expect("startup response")
    }

    #[rstest]
    #[case::beyond_legacy_gap(301, true)]
    #[case::within_catchup_window(2_999, true)]
    #[case::beyond_catchup_window(3_001, false)]
    #[tokio::test]
    async fn regular_gossip_gap_spans_the_catchup_window(#[case] gap: u64, #[case] admitted: bool) {
        let coordinator = coordinator(false, false, None);
        let client = Arc::clone(coordinator.processor.client());
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = coordinator.start(request_rx);

        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(payload(gap, 1))))
            .await
            .expect("gossip request");
        // Requests are handled in order, so a following barrier means the gossip was handled.
        let (result_tx, mut result_rx) = mpsc::channel(1);
        request_tx
            .send(EngineActorRequest::PrepareSequencerStart {
                expected_hash: B256::with_last_byte(1),
                result_tx,
            })
            .await
            .expect("request barrier");
        // The reply may be deferred while an admitted insert is in flight; only ordering matters.
        let _ = tokio::time::timeout(Duration::from_millis(500), result_rx.recv()).await;
        // An admitted insert reaches the engine asynchronously.
        for _ in 0..300 {
            if client.last_new_payload_v2().await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();

        assert_eq!(client.last_new_payload_v2().await.is_some(), admitted);
    }

    #[rstest]
    #[case::stale(99)]
    #[case::same_height_different_hash(100)]
    #[tokio::test(start_paused = true)]
    async fn isolated_probe_never_rewinds_bootstrapped_head(#[case] number: u64) {
        let mut coordinator = isolated_startup_coordinator();
        let client = Arc::clone(coordinator.processor.client());
        let state_rx = coordinator.processor.engine_mut().state_subscribe();
        client
            .set_fork_choice_updated_v3_response(ForkchoiceUpdated {
                payload_status: PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: None,
                },
                payload_id: None,
            })
            .await;
        let mut gossip = payload(number, 1);
        let block = gossip.execution_payload.clone().try_into_block::<BaseTxEnvelope>().unwrap();
        gossip.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = coordinator.start(request_rx);
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip)))
            .await
            .unwrap();
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        let bootstrapped_head = state_rx.borrow().sync_state.unsafe_head();

        tokio::time::sleep(Duration::from_secs(6)).await;
        let activation = request_start(&request_tx).await;
        drop(request_tx);
        handle.await.unwrap().expect_err("closed request channel");

        assert!(client.storage().read().await.fork_choice_updated_v3_requests.is_empty());
        assert_eq!(state_rx.borrow().sync_state.unsafe_head(), bootstrapped_head);
        assert!(matches!(activation, Err(EngineClientError::ELSyncing)));
    }

    #[rstest]
    #[case::already_at_tip(true)]
    #[case::sync_finishes_after_last_gossip(false)]
    #[tokio::test(start_paused = true)]
    async fn isolated_activates_without_further_gossip(#[case] already_at_tip: bool) {
        let coordinator = isolated_startup_coordinator();
        let client = Arc::clone(coordinator.processor.client());
        let mut gossip = payload(500, 1);
        let block = gossip.execution_payload.clone().try_into_block::<BaseTxEnvelope>().unwrap();
        gossip.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let head =
            L2BlockInfo::from_block_and_genesis(&block, &RollupConfig::default().genesis).unwrap();
        if already_at_tip {
            client.set_block_info_by_tag(BlockNumberOrTag::Latest, head).await;
        }
        client
            .set_new_payload_v2_response(PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            })
            .await;
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = coordinator.start(request_rx);
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip)))
            .await
            .unwrap();
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));

        client
            .set_fork_choice_updated_v3_response(ForkchoiceUpdated {
                payload_status: PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: None,
                },
                payload_id: None,
            })
            .await;
        // Allow more than the five-second probe interval without sending further gossip.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let activation = request_start(&request_tx).await;
        drop(request_tx);
        handle.await.unwrap().expect_err("closed request channel");
        activation.expect("EL has the observed canonical tip and should allow isolated startup");
    }

    #[tokio::test(start_paused = true)]
    async fn isolated_startup_inserts_gossip_and_defers_until_el_synced_head() {
        let mut coordinator = isolated_startup_coordinator();
        let client = Arc::clone(coordinator.processor.client());
        let state_rx = coordinator.processor.engine_mut().state_subscribe();
        let mut queue_rx = coordinator.processor.engine_mut().queue_length_subscribe();
        let mut gossip = payload(500, 1);
        let block = gossip.execution_payload.clone().try_into_block::<BaseTxEnvelope>().unwrap();
        gossip.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = coordinator.start(request_rx);

        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        assert!(!state_rx.borrow().el_sync_finished);
        assert!(client.storage().read().await.fork_choice_updated_v3_requests.is_empty());

        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip.clone())))
            .await
            .expect("gossip request");
        tokio::time::timeout(Duration::from_secs(5), queue_rx.wait_for(|length| *length == 1))
            .await
            .expect("external insert was not queued")
            .expect("queue watch closed");
        client
            .set_new_payload_v2_response(PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            })
            .await;
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        assert!(client.last_new_payload_v2().await.is_some());
        assert!(!state_rx.borrow().el_sync_finished);

        client
            .set_fork_choice_updated_v3_response(ForkchoiceUpdated {
                payload_status: PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: None,
                },
                payload_id: None,
            })
            .await;
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip.clone())))
            .await
            .expect("synced gossip request");
        request_start(&request_tx).await.expect("isolated activation");
        let head = state_rx.borrow().sync_state.unsafe_head();
        assert!(state_rx.borrow().el_sync_finished);
        assert_eq!(
            (head.block_info.number, head.block_info.hash),
            (500, gossip.execution_payload.block_hash())
        );

        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(payload(501, 2))))
            .await
            .expect("post-activation gossip request");
        let (result_tx, mut result_rx) = mpsc::channel(1);
        request_tx
            .send(EngineActorRequest::PrepareSequencerStart {
                expected_hash: head.block_info.hash,
                result_tx,
            })
            .await
            .expect("request barrier");
        result_rx.recv().await.expect("barrier response").expect("idempotent activation");
        assert_eq!(state_rx.borrow().sync_state.unsafe_head(), head);
        assert_eq!(*queue_rx.borrow(), 0);
        let fcu_count = client.storage().read().await.fork_choice_updated_v3_requests.len();
        tokio::time::sleep(Duration::from_secs(6)).await;
        drop(request_tx);
        handle.await.expect("engine task").expect_err("closed request channel");
        assert_eq!(client.storage().read().await.fork_choice_updated_v3_requests.len(), fcu_count);
    }

    #[tokio::test(start_paused = true)]
    async fn isolated_probe_retries_newer_gossip_until_valid() {
        let mut coordinator = isolated_startup_coordinator();
        let client = Arc::clone(coordinator.processor.client());
        let state_rx = coordinator.processor.engine_mut().state_subscribe();
        client
            .set_new_payload_v2_response(PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            })
            .await;
        let mut gossip = payload(500, 1);
        let block = gossip.execution_payload.clone().try_into_block::<BaseTxEnvelope>().unwrap();
        gossip.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = coordinator.start(request_rx);
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip)))
            .await
            .unwrap();
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        let mut gossip = payload(501, 2);
        let block = gossip.execution_payload.clone().try_into_block::<BaseTxEnvelope>().unwrap();
        gossip.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let target_hash = gossip.execution_payload.block_hash();
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip)))
            .await
            .unwrap();
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        client
            .set_fork_choice_updated_v3_response(ForkchoiceUpdated {
                payload_status: PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: None,
                },
                payload_id: None,
            })
            .await;
        tokio::time::sleep(Duration::from_secs(6)).await;
        let activation = request_start(&request_tx).await;
        drop(request_tx);
        handle.await.unwrap().expect_err("closed request channel");
        activation.expect("probe must confirm the newer gossip target");
        assert_eq!(state_rx.borrow().sync_state.unsafe_head().block_info.hash, target_hash);
    }

    #[tokio::test(start_paused = true)]
    async fn isolated_conflict_stops_probe_retries() {
        let coordinator = isolated_startup_coordinator();
        let client = Arc::clone(coordinator.processor.client());
        client
            .set_new_payload_v2_response(PayloadStatus {
                status: PayloadStatusEnum::Valid,
                latest_valid_hash: None,
            })
            .await;
        let mut gossip = payload(500, 1);
        let mut block =
            gossip.execution_payload.clone().try_into_block::<BaseTxEnvelope>().unwrap();
        gossip.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let mut conflicting = gossip.clone();
        block.header.timestamp += 1;
        conflicting.execution_payload =
            BaseExecutionPayload::from_block_unchecked(block.hash_slow(), &block).0;
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = coordinator.start(request_rx);
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(gossip)))
            .await
            .unwrap();
        assert!(matches!(request_start(&request_tx).await, Err(EngineClientError::ELSyncing)));
        tokio::time::sleep(Duration::from_secs(6)).await;
        request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(conflicting)))
            .await
            .unwrap();
        assert!(matches!(
            request_start(&request_tx).await,
            Err(EngineClientError::ShadowBufferFaulted)
        ));
        let fcu_count = client.storage().read().await.fork_choice_updated_v3_requests.len();
        tokio::time::sleep(Duration::from_secs(6)).await;
        drop(request_tx);
        handle.await.unwrap().expect_err("closed request channel");
        assert_eq!(client.storage().read().await.fork_choice_updated_v3_requests.len(), fcu_count);
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
        assert!(matches!(coordinator.sequencer_state(), SequencerEngineState::Regular));
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
    #[case::faulted_and_el_syncing(100, false, &[(100, 100), (100, 99)], false, false)]
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
        let mode = if shadow {
            NodeOperatingMode::ShadowSequencer { blocks_per_cycle: std::num::NonZeroU64::MIN }
        } else {
            NodeOperatingMode::Sequencer
        };
        let mut coordinator =
            SequencerEngineRequestCoordinator::new(processor, mode, None, true, head_tx);
        let mut catchup = CanonicalUnsafeCatchup::default();
        for &(number, hash) in observations {
            catchup.buffer_payload(payload(number, hash));
        }
        catchup.commit(head);
        *coordinator.sequencer_state_mut() = SequencerEngineState::CatchingUp { catchup };

        let result = coordinator.prepare_sequencer_start(B256::with_last_byte(requested_hash));
        assert_eq!(result.is_ok(), accepted, "{result:?}");
        if !shadow && requested_hash == 100 && !el_synced {
            assert!(matches!(result, Err(EngineClientError::ELSyncing)));
        }
        assert_eq!(
            matches!(coordinator.sequencer_state(), SequencerEngineState::Regular),
            accepted && !shadow
        );
        // Shadow starts never leave catch-up; other starts leave it only when accepted.
        assert_eq!(
            matches!(coordinator.sequencer_state(), SequencerEngineState::CatchingUp { .. }),
            shadow || !accepted
        );
        // Acceptance must remain idempotent; refusal must not discard the blocking evidence.
        assert_eq!(
            coordinator.prepare_sequencer_start(B256::with_last_byte(requested_hash)).is_ok(),
            accepted
        );

        if shadow {
            *coordinator.sequencer_state_mut() =
                SequencerEngineState::ShadowActive(Box::new(ShadowReconciliationGate::new(head)));
            assert_eq!(
                coordinator.prepare_sequencer_start(B256::with_last_byte(requested_hash)).is_ok(),
                requested_hash == 100
            );
            assert!(matches!(coordinator.sequencer_state(), SequencerEngineState::ShadowActive(_)));
        }
    }

    #[rstest]
    #[case::caught_up(&[(100, 100)], true)]
    #[case::gap(&[(102, 102)], false)]
    #[case::no_gossip(&[], false)]
    #[case::wrong_hash(&[(100, 99)], false)]
    #[case::stale_gossip(&[(99, 99)], false)]
    #[case::conflict(&[(100, 100), (100, 99)], false)]
    #[tokio::test]
    async fn isolated_activation_requires_el_synced_canonical_head(
        #[case] observations: &[(u64, u8)],
        #[case] accepted: bool,
        #[values(false, true)] el_synced: bool,
        #[values(false, true)] startup_reset: bool,
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
        let mut coordinator = SequencerEngineRequestCoordinator::new(
            processor,
            NodeOperatingMode::IsolatedSequencer,
            None,
            true,
            head_tx,
        );
        let SequencerEngineState::CatchingUp { catchup } = coordinator.sequencer_state_mut() else {
            panic!("isolated coordinator must start in catch-up");
        };
        for &(number, hash) in observations {
            catchup.observe_payload(&payload(number, hash));
        }
        let faulted = catchup.is_faulted();

        let accepted = accepted && el_synced;
        let queue_rx = coordinator.processor.engine_mut().queue_length_subscribe();
        coordinator.processor.enqueue(EngineTask::Consolidate(Box::new(ConsolidateTask::new(
            Arc::clone(coordinator.processor.client()),
            Arc::clone(coordinator.processor.rollup()),
            TestAttributesBuilder::new().with_parent(head).build(),
        ))));
        let result = if startup_reset {
            let (result_tx, mut result_rx) = mpsc::channel(1);
            coordinator
                .handle_reset_request(ResetRequest {
                    result_tx,
                    origin: ResetOrigin::Sequencer,
                    reason: ResetReason::SequencerStartup,
                })
                .await
                .expect("startup activation must not reset the canonical parent");
            result_rx.recv().await.expect("startup response")
        } else {
            coordinator.prepare_sequencer_start(head.block_info.hash)
        };
        assert_eq!(result.is_ok(), accepted);
        if faulted {
            assert!(matches!(result, Err(EngineClientError::ShadowBufferFaulted)));
        }
        assert_eq!(*queue_rx.borrow(), usize::from(!accepted));
        assert_eq!(coordinator.processor.engine_state().sync_state.unsafe_head(), head);
        assert_eq!(
            matches!(coordinator.sequencer_state(), SequencerEngineState::IsolatedActive),
            accepted
        );
        if accepted {
            assert!(coordinator.prepare_sequencer_start(head.block_info.hash).is_ok());
            assert!(matches!(coordinator.sequencer_state(), SequencerEngineState::IsolatedActive));
        }
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
