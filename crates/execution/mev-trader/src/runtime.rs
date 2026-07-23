//! Receive-only A1 runtime ownership, latest-wins isolation, and terminal accounting.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use base_execution_chainspec::BaseChainSpec;
use thiserror::Error;
use tokio::{sync::Notify, time};

use crate::{
    A1Counters, A1Outcome, A1Status, BackrunPlan, BlinkVictim, CancellationProbe,
    CancellationToken, DedicatedAnalysisPool, FixturePoolRegistry, FrameAuditPlan, FrameProcessor,
    GlobalLifecycle, GlobalState, LatestSlot, LifecycleError, MeasurementContext, PairwiseEngine,
    PairwiseError, PoolDescriptor, PoolStatePreparer, PoolUniverseSnapshot, PortError,
    PreparationError, QueuedBlinkVictim, RegistryDigest, RegistryError, RegistryHasher,
    RuntimeShutdown, ShadowLatestSlot, ShadowSlotCounters, SlotSubmit, SnapshotCaptureCoordinator,
    SoleWorker, TaskRun, TaskRunner, TaskState, TraderSnapshotPort, VictimFrame, Watchdog,
    WatchdogStatus, WorkerClaim,
};

const TERMINAL_UNCLAIMED: u8 = 0;
const TERMINAL_FRAME_BOUND: u8 = 1;
const TERMINAL_NO_TRADE: u8 = 2;
const TERMINAL_CANCELLED: u8 = 3;
const TERMINAL_INTERNAL_FAILURE: u8 = 4;
const CONTROL_INTERVAL: Duration = Duration::from_millis(5);

/// Exact-parent node-local code-hash admission for a complete pool universe.
#[derive(Debug, Default, Clone, Copy)]
pub struct PoolCodeHashView;

impl PoolCodeHashView {
    /// Opens one exact-parent state view and validates every configured pool against it.
    pub fn validate(
        port: &dyn TraderSnapshotPort,
        snapshot: &crate::SnapshotHandle,
        universe: &PoolUniverseSnapshot,
        probe: &CancellationProbe,
    ) -> bool {
        let parent_hash = snapshot.parent_hash();
        let Ok(provider) = port.state_at_hash(parent_hash) else {
            return false;
        };
        Self::validate_at_parent(
            parent_hash,
            universe.descriptors(),
            || port.is_current_authoritative(snapshot),
            |_, pool| {
                if !probe.checkpoint(Instant::now(), port.is_current_authoritative(snapshot)) {
                    return Err(PortError::Incoherent);
                }
                provider
                    .basic_account(&pool)
                    .map(|account| account.and_then(|account| account.bytecode_hash))
                    .map_err(|_| PortError::ProviderUnavailable)
            },
        )
    }

    /// Validates observed code hashes while pinning every read to one parent hash.
    pub fn validate_at_parent<Authority, Reader>(
        parent_hash: B256,
        descriptors: &[PoolDescriptor],
        mut authoritative: Authority,
        mut read_code_hash: Reader,
    ) -> bool
    where
        Authority: FnMut() -> bool,
        Reader: FnMut(B256, Address) -> Result<Option<B256>, PortError>,
    {
        if descriptors.is_empty() {
            return false;
        }
        for descriptor in descriptors {
            if !authoritative()
                || read_code_hash(parent_hash, descriptor.pool).ok().flatten()
                    != Some(descriptor.code_hash)
            {
                return false;
            }
        }
        authoritative()
    }
}

/// Typed outcome of one measurement-only T4a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowOutcome {
    /// Victim execution or delta admission rejected the frame.
    FrameRejected,
    /// Exact-parent pool code-hash admission rejected the frame.
    CodeHashRejected,
    /// Provider-free pool-state preparation rejected the frame.
    PreparationRejected,
    /// V3 tick/bitmap coverage was incomplete or inconsistent.
    V3Coverage,
    /// Pairwise discovery or selection rejected analysis without a partial result.
    AnalysisRejected,
    /// Panic or watchdog hang disabled the runtime.
    InternalFailure,
    /// The shared frame cancellation domain stopped analysis.
    Cancelled,
    /// No actual quote-bearing pool changed.
    NoDirtyPools,
    /// Pairwise discovery found no candidate.
    NoCandidate,
    /// Candidates existed but none had positive gross output.
    NoPositivePlan,
    /// Exactly one positive-gross measurement plan was selected.
    Selected,
}

/// Atomic terminal accounting for every enabled T4a frame outcome.
#[derive(Debug, Default)]
pub struct ShadowOutcomeCounters {
    frame_rejected: AtomicU64,
    code_hash_rejected: AtomicU64,
    preparation_rejected: AtomicU64,
    v3_coverage: AtomicU64,
    analysis_rejected: AtomicU64,
    internal_failure: AtomicU64,
    cancelled: AtomicU64,
    no_dirty_pools: AtomicU64,
    no_candidate: AtomicU64,
    no_positive_plan: AtomicU64,
    selected: AtomicU64,
}

impl ShadowOutcomeCounters {
    /// Records one typed terminal outcome.
    pub fn record(&self, outcome: ShadowOutcome) {
        let counter = match outcome {
            ShadowOutcome::FrameRejected => &self.frame_rejected,
            ShadowOutcome::CodeHashRejected => &self.code_hash_rejected,
            ShadowOutcome::PreparationRejected => &self.preparation_rejected,
            ShadowOutcome::V3Coverage => &self.v3_coverage,
            ShadowOutcome::AnalysisRejected => &self.analysis_rejected,
            ShadowOutcome::InternalFailure => &self.internal_failure,
            ShadowOutcome::Cancelled => &self.cancelled,
            ShadowOutcome::NoDirtyPools => &self.no_dirty_pools,
            ShadowOutcome::NoCandidate => &self.no_candidate,
            ShadowOutcome::NoPositivePlan => &self.no_positive_plan,
            ShadowOutcome::Selected => &self.selected,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the count for one typed terminal outcome.
    pub fn count(&self, outcome: ShadowOutcome) -> u64 {
        let counter = match outcome {
            ShadowOutcome::FrameRejected => &self.frame_rejected,
            ShadowOutcome::CodeHashRejected => &self.code_hash_rejected,
            ShadowOutcome::PreparationRejected => &self.preparation_rejected,
            ShadowOutcome::V3Coverage => &self.v3_coverage,
            ShadowOutcome::Cancelled => &self.cancelled,
            ShadowOutcome::AnalysisRejected => &self.analysis_rejected,
            ShadowOutcome::InternalFailure => &self.internal_failure,
            ShadowOutcome::NoDirtyPools => &self.no_dirty_pools,
            ShadowOutcome::NoCandidate => &self.no_candidate,
            ShadowOutcome::NoPositivePlan => &self.no_positive_plan,
            ShadowOutcome::Selected => &self.selected,
        };
        counter.load(Ordering::Relaxed)
    }
}

/// Capacity-one detail envelope for one frame-bound shadow measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowFrameMeasurement {
    /// Exact frame identity.
    pub context: MeasurementContext,
    /// Canonical immutable pool-universe digest.
    pub registry_digest: RegistryDigest,
    /// Number of actual dirty pools.
    pub dirty_pool_count: u32,
    /// Number of fully prepared pools.
    pub prepared_pool_count: u32,
    /// Number of discovered candidates.
    pub discovered_candidate_count: u32,
    /// Typed terminal measurement outcome.
    pub outcome: ShadowOutcome,
    /// At most one positive-gross measurement-only plan.
    pub plan: Option<BackrunPlan>,
}

/// Failures while constructing the empty, provider-free Phase A runtime.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeInstallError {
    /// Empty registry digest construction or validation failed.
    #[error("empty Phase A registry construction failed")]
    Registry,
    /// Sole-worker or dedicated Rayon4 construction failed.
    #[error("Phase A lifecycle construction failed")]
    Lifecycle,
}

impl From<RegistryError> for RuntimeInstallError {
    fn from(_error: RegistryError) -> Self {
        Self::Registry
    }
}

impl From<LifecycleError> for RuntimeInstallError {
    fn from(_error: LifecycleError) -> Self {
        Self::Lifecycle
    }
}

/// Exact empty-registry configuration used by the receive-only runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MevTraderRuntimeConfig {
    registry: FixturePoolRegistry,
    universe: Option<PoolUniverseSnapshot>,
}

impl MevTraderRuntimeConfig {
    /// Constructs and validates the canonical empty fixture registry.
    pub fn empty() -> Result<Self, RuntimeInstallError> {
        let descriptors = Vec::new();
        let digest = RegistryHasher::digest(&descriptors)?;
        let registry = FixturePoolRegistry::new(descriptors, digest)?;
        Ok(Self { registry, universe: None })
    }

    /// Constructs an enabled measurement-only runtime from one validated immutable snapshot.
    pub fn shadow(snapshot: PoolUniverseSnapshot) -> Result<Self, RuntimeInstallError> {
        let descriptors = snapshot.descriptors().to_vec();
        let registry = FixturePoolRegistry::new(descriptors, snapshot.registry_digest())?;
        Ok(Self { registry, universe: Some(snapshot) })
    }

    /// Returns whether the measurement-only pool universe is disabled.
    pub const fn is_empty(&self) -> bool {
        self.universe.is_none()
    }
}

/// Sole receive-only runtime owner for ingress, consumer, control, and Rayon4 state.
#[derive(Debug)]
pub struct MevTraderRuntime {
    registry: FixturePoolRegistry,
    universe: Option<PoolUniverseSnapshot>,
    lifecycle: Arc<GlobalLifecycle>,
    ingress: Arc<LatestSlot<QueuedBlinkVictim>>,
    shadow: ShadowLatestSlot<ShadowFrameMeasurement>,
    analysis: DedicatedAnalysisPool,
    worker: WorkerClaim,
    watchdog: Watchdog,
    counters: Arc<A1Counters>,
    shadow_outcomes: ShadowOutcomeCounters,
    shutdown: Arc<RuntimeShutdown>,
    status: AtomicU8,
    generation: AtomicU64,
    slot_notify: Notify,
    control_notify: Notify,
    active: Mutex<Option<(u64, Arc<CancellationToken>)>>,
    active_terminal: Mutex<Option<(u64, Arc<AtomicU8>)>>,
}

impl MevTraderRuntime {
    /// Starts the empty registry, capacity-one slot, sole consumer, control, and Rayon4 domains.
    pub fn start(config: MevTraderRuntimeConfig) -> Result<Self, RuntimeInstallError> {
        let lifecycle = Arc::new(GlobalLifecycle::default());
        let ingress = Arc::new(LatestSlot::new(Arc::clone(&lifecycle)));
        let worker = SoleWorker::default().claim()?;
        let analysis = DedicatedAnalysisPool::new()?;
        Ok(Self {
            registry: config.registry,
            universe: config.universe,
            lifecycle,
            ingress,
            shadow: ShadowLatestSlot::new(),
            analysis,
            worker,
            watchdog: Watchdog,
            counters: Arc::new(A1Counters::default()),
            shadow_outcomes: ShadowOutcomeCounters::default(),
            shutdown: Arc::new(RuntimeShutdown::default()),
            status: AtomicU8::new(A1Status::Off as u8),
            generation: AtomicU64::new(0),
            slot_notify: Notify::new(),
            control_notify: Notify::new(),
            active: Mutex::new(None),
            active_terminal: Mutex::new(None),
        })
    }

    /// Returns true only while the production registry remains empty.
    pub const fn registry_is_empty(&self) -> bool {
        self.registry.is_empty()
    }

    /// Returns the exact dedicated Rayon4 thread count.
    pub fn analysis_thread_count(&self) -> usize {
        self.analysis.thread_count()
    }

    /// Proves that the irreversible sole-consumer claim is held.
    pub const fn worker_is_claimed(&self) -> bool {
        self.worker.marker();
        true
    }

    /// Returns the current closed A1 status.
    pub fn a1_status(&self) -> A1Status {
        A1Status::from_u8(self.status.load(Ordering::SeqCst))
    }

    /// Applies a status transition without reopening a terminal state.
    pub fn set_a1_status(&self, next: A1Status) {
        let mut current = self.status.load(Ordering::SeqCst);
        loop {
            if A1Status::from_u8(current).is_terminal() {
                return;
            }
            match self.status.compare_exchange(
                current,
                next as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns the fixed counters shared by ingress and runtime control.
    pub const fn counters(&self) -> &Arc<A1Counters> {
        &self.counters
    }

    /// Returns measurement-slot acceptance, replacement, drop, drain, and shutdown counters.
    pub const fn shadow_slot_counters(&self) -> &ShadowSlotCounters {
        self.shadow.counters()
    }

    /// Returns typed terminal accounting for enabled T4a frames.
    pub const fn shadow_outcome_counters(&self) -> &ShadowOutcomeCounters {
        &self.shadow_outcomes
    }

    /// Records one closed A1 outcome without retaining source data.
    pub fn record_a1(&self, outcome: A1Outcome) {
        self.counters.record(outcome);
    }

    /// Returns the root-cancellation domain.
    pub const fn shutdown(&self) -> &Arc<RuntimeShutdown> {
        &self.shutdown
    }

    /// Assigns a checked generation and performs capacity-one latest-wins submission.
    pub(crate) fn submit_blink_victim(&self, victim: BlinkVictim) {
        if self.shutdown.is_cancelled() || self.lifecycle.state() != GlobalState::Running {
            self.record_a1(A1Outcome::SlotClosed);
            return;
        }
        let generation =
            match self.generation.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |generation| {
                generation.checked_add(1)
            }) {
                Ok(previous) => previous + 1,
                Err(_) => {
                    self.record_a1(A1Outcome::GenerationOverflow);
                    self.set_a1_status(A1Status::DisabledPermanent);
                    self.lifecycle.close();
                    self.shutdown.cancel();
                    self.slot_notify.notify_waiters();
                    self.control_notify.notify_waiters();
                    return;
                }
            };

        if let Some((active_generation, token)) =
            self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().cloned()
            && active_generation < generation
        {
            token.request_cancel();
            self.control_notify.notify_one();
        }

        let outcome = match self.ingress.submit(QueuedBlinkVictim::new(generation, victim)) {
            SlotSubmit::Accepted => A1Outcome::SlotAccepted,
            SlotSubmit::Replaced => A1Outcome::SlotReplaced,
            SlotSubmit::Closed => A1Outcome::SlotClosed,
        };
        self.record_a1(outcome);
        if outcome != A1Outcome::SlotClosed {
            self.slot_notify.notify_one();
        }
    }

    /// Consumes at most one generation through capture, frame binding, Rayon4, and terminal claim.
    pub fn consume_once(
        &self,
        port: &dyn TraderSnapshotPort,
        chain_spec: Arc<BaseChainSpec>,
    ) -> bool {
        let Some(queued) = self.ingress.try_take() else { return false };
        let generation = queued.generation();
        let victim = queued.into_victim();
        let deadline = victim.received_at() + Duration::from_millis(crate::DEADLINE_MILLIS);
        let token = Arc::new(CancellationToken::new(deadline));
        let terminal = Arc::new(AtomicU8::new(TERMINAL_UNCLAIMED));
        {
            let mut active = self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((_, older)) = active.replace((generation, Arc::clone(&token))) {
                older.request_cancel();
            }
        }
        *self.active_terminal.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((generation, Arc::clone(&terminal)));

        if self.shutdown.is_cancelled()
            || self.lifecycle.state() != GlobalState::Running
            || self.generation.load(Ordering::SeqCst) != generation
            || Instant::now() >= token.deadline()
        {
            self.cancel_and_claim(generation, &token, &terminal);
            self.clear_active(generation, &token, &terminal);
            return true;
        }

        let snapshot = match SnapshotCaptureCoordinator.capture(port) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None)
            | Err(PortError::SnapshotUnavailable)
            | Err(PortError::ProviderUnavailable)
            | Err(PortError::HeaderUnavailable)
            | Err(PortError::VisitorStopped)
            | Err(PortError::LimitExceeded)
            | Err(PortError::Incoherent)
            | Err(PortError::FactoryAlreadyUsed) => {
                let current = self.generation.load(Ordering::SeqCst) == generation;
                if token.complete(Instant::now(), current, &self.lifecycle) {
                    self.claim_terminal(
                        generation,
                        &terminal,
                        A1Outcome::NoTrade,
                        TERMINAL_NO_TRADE,
                        Some(ShadowOutcome::FrameRejected),
                    );
                } else {
                    self.cancel_and_claim(generation, &token, &terminal);
                }
                self.clear_active(generation, &token, &terminal);
                return true;
            }
        };

        let frame = VictimFrame {
            chain_id: victim.chain_id(),
            transaction_type: victim.transaction_type(),
            transaction_hash: victim.hash(),
            from: victim.from(),
            raw_tx: victim.raw_tx().clone(),
            parent_hash: snapshot.parent_hash(),
            block_number: victim.block_number(),
            victim_flashblock_index: victim.flashblock_index(),
            received_at: victim.received_at(),
        };
        let probe = CancellationProbe::new(Arc::clone(&token), Arc::clone(&self.lifecycle));
        let run = TaskRunner.run(&self.lifecycle, || {
            self.analysis.install(&probe, |probe| {
                let Some(universe) = self.universe.as_ref() else {
                    let audit = FrameAuditPlan::new(Vec::new(), BTreeMap::new())
                        .map_err(|_| PortError::Incoherent)?;
                    return FrameProcessor::process(
                        port,
                        &snapshot,
                        &frame,
                        Instant::now(),
                        chain_spec,
                        &audit,
                        probe,
                    )
                    .map(|processed| (processed, None, None));
                };

                if !PoolCodeHashView::validate(port, &snapshot, universe, probe) {
                    let outcome = if probe
                        .checkpoint(Instant::now(), port.is_current_authoritative(&snapshot))
                    {
                        ShadowOutcome::CodeHashRejected
                    } else {
                        ShadowOutcome::Cancelled
                    };
                    return Ok((None, None, Some(outcome)));
                }
                let Some(processed) = FrameProcessor::process(
                    port,
                    &snapshot,
                    &frame,
                    Instant::now(),
                    chain_spec,
                    universe.audit(),
                    probe,
                )?
                else {
                    let outcome = if probe
                        .checkpoint(Instant::now(), port.is_current_authoritative(&snapshot))
                    {
                        ShadowOutcome::FrameRejected
                    } else {
                        ShadowOutcome::Cancelled
                    };
                    return Ok((None, None, Some(outcome)));
                };

                let context = *processed.measurement_context();
                let dirty_pool_count = u32::try_from(processed.dirty_pools().len())
                    .map_err(|_| PortError::LimitExceeded)?;
                let prepared = match PoolStatePreparer::prepare(
                    universe,
                    processed.materialized_state(),
                    probe,
                ) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let outcome = match error {
                            PreparationError::V3Coverage => ShadowOutcome::V3Coverage,
                            PreparationError::Cancelled => ShadowOutcome::Cancelled,
                            _ => ShadowOutcome::PreparationRejected,
                        };
                        let measurement = ShadowFrameMeasurement {
                            context,
                            registry_digest: universe.registry_digest(),
                            dirty_pool_count,
                            prepared_pool_count: 0,
                            discovered_candidate_count: 0,
                            outcome,
                            plan: None,
                        };
                        return Ok((Some(processed), Some(measurement), None));
                    }
                };
                let prepared_pool_count =
                    u32::try_from(prepared.len()).map_err(|_| PortError::LimitExceeded)?;
                let fixture_id = format!(
                    "t4a_{}_{}_{:x}",
                    context.block_number, context.predecessor_index, context.victim
                );
                let candidates = match PairwiseEngine::discover(
                    &fixture_id,
                    &prepared,
                    processed.dirty_pools().as_slice(),
                    probe,
                ) {
                    Ok(candidates) => candidates,
                    Err(error) => {
                        let outcome = if error == PairwiseError::Cancelled {
                            ShadowOutcome::Cancelled
                        } else {
                            ShadowOutcome::AnalysisRejected
                        };
                        let measurement = ShadowFrameMeasurement {
                            context,
                            registry_digest: universe.registry_digest(),
                            dirty_pool_count,
                            prepared_pool_count,
                            discovered_candidate_count: 0,
                            outcome,
                            plan: None,
                        };
                        return Ok((Some(processed), Some(measurement), None));
                    }
                };
                let discovered_candidate_count =
                    u32::try_from(candidates.len()).map_err(|_| PortError::LimitExceeded)?;
                let plan = match PairwiseEngine::select_measurement(&processed, &candidates, probe)
                {
                    Ok(plan) => plan,
                    Err(error) => {
                        let outcome = if error == PairwiseError::Cancelled {
                            ShadowOutcome::Cancelled
                        } else {
                            ShadowOutcome::AnalysisRejected
                        };
                        let measurement = ShadowFrameMeasurement {
                            context,
                            registry_digest: universe.registry_digest(),
                            dirty_pool_count,
                            prepared_pool_count,
                            discovered_candidate_count,
                            outcome,
                            plan: None,
                        };
                        return Ok((Some(processed), Some(measurement), None));
                    }
                };
                let outcome = if processed.dirty_pools().is_empty() {
                    ShadowOutcome::NoDirtyPools
                } else if candidates.is_empty() {
                    ShadowOutcome::NoCandidate
                } else if plan.is_some() {
                    ShadowOutcome::Selected
                } else {
                    ShadowOutcome::NoPositivePlan
                };
                let measurement = ShadowFrameMeasurement {
                    context,
                    registry_digest: universe.registry_digest(),
                    dirty_pool_count,
                    prepared_pool_count,
                    discovered_candidate_count,
                    outcome,
                    plan,
                };
                Ok((Some(processed), Some(measurement), None))
            })
        });
        match run {
            TaskRun::Panicked => {
                self.claim_terminal(
                    generation,
                    &terminal,
                    A1Outcome::InternalFailure,
                    TERMINAL_INTERNAL_FAILURE,
                    Some(ShadowOutcome::InternalFailure),
                );
                token.request_cancel();
                self.set_a1_status(A1Status::DisabledPermanent);
                self.shutdown.cancel();
                self.slot_notify.notify_waiters();
                self.control_notify.notify_waiters();
            }
            TaskRun::Returned(None) => {
                self.cancel_and_claim(generation, &token, &terminal);
            }
            TaskRun::Returned(Some(Err(
                PortError::SnapshotUnavailable
                | PortError::ProviderUnavailable
                | PortError::HeaderUnavailable
                | PortError::VisitorStopped
                | PortError::LimitExceeded
                | PortError::Incoherent
                | PortError::FactoryAlreadyUsed,
            ))) => {
                let current = self.generation.load(Ordering::SeqCst) == generation
                    && port.is_current_authoritative(&snapshot);
                if token.complete(Instant::now(), current, &self.lifecycle) {
                    self.claim_terminal(
                        generation,
                        &terminal,
                        A1Outcome::NoTrade,
                        TERMINAL_NO_TRADE,
                        Some(ShadowOutcome::FrameRejected),
                    );
                } else {
                    self.cancel_and_claim(generation, &token, &terminal);
                }
            }
            TaskRun::Returned(Some(Ok((None, _, analysis_outcome)))) => {
                let current = self.generation.load(Ordering::SeqCst) == generation
                    && port.is_current_authoritative(&snapshot);
                if token.complete(Instant::now(), current, &self.lifecycle) {
                    self.claim_terminal(
                        generation,
                        &terminal,
                        A1Outcome::NoTrade,
                        TERMINAL_NO_TRADE,
                        Some(analysis_outcome.unwrap_or(ShadowOutcome::FrameRejected)),
                    );
                } else {
                    self.cancel_and_claim(generation, &token, &terminal);
                }
            }
            TaskRun::Returned(Some(Ok((Some(_processed), measurement, _)))) => {
                let current = self.generation.load(Ordering::SeqCst) == generation
                    && port.is_current_authoritative(&snapshot);
                if token.complete(Instant::now(), current, &self.lifecycle) {
                    let shadow_outcome = measurement
                        .as_ref()
                        .map(|measurement| measurement.outcome)
                        .or_else(|| self.universe.as_ref().map(|_| ShadowOutcome::FrameRejected));
                    if self.claim_terminal(
                        generation,
                        &terminal,
                        A1Outcome::FrameBound,
                        TERMINAL_FRAME_BOUND,
                        shadow_outcome,
                    ) && let Some(measurement) = measurement
                    {
                        self.shadow.try_submit(measurement);
                        self.control_notify.notify_one();
                    }
                } else {
                    self.cancel_and_claim(generation, &token, &terminal);
                }
            }
        }
        self.clear_active(generation, &token, &terminal);
        true
    }

    /// Runs the sole notified consumer until root shutdown.
    pub async fn run_consumer(
        self: Arc<Self>,
        port: Arc<dyn TraderSnapshotPort>,
        chain_spec: Arc<BaseChainSpec>,
    ) {
        loop {
            while self.consume_once(port.as_ref(), Arc::clone(&chain_spec)) {}
            if self.shutdown.is_cancelled() {
                return;
            }
            let notified = self.slot_notify.notified();
            if self.consume_once(port.as_ref(), Arc::clone(&chain_spec)) {
                continue;
            }
            tokio::select! {
                () = self.shutdown.wait_cancelled() => return,
                () = notified => {}
            }
        }
    }

    /// Runs the one independent watchdog/control domain until root shutdown.
    pub async fn run_control(self: Arc<Self>) {
        loop {
            tokio::select! {
                () = self.shutdown.wait_cancelled() => return,
                () = self.control_notify.notified() => {},
                () = time::sleep(CONTROL_INTERVAL) => {},
            }
            self.inspect_watchdog();
            if let Some(measurement) = self.shadow.try_take() {
                tracing::debug!(
                    block_number = measurement.context.block_number,
                    predecessor_index = measurement.context.predecessor_index,
                    victim = %measurement.context.victim,
                    registry_digest = ?measurement.registry_digest,
                    dirty_pool_count = measurement.dirty_pool_count,
                    prepared_pool_count = measurement.prepared_pool_count,
                    discovered_candidate_count = measurement.discovered_candidate_count,
                    outcome = ?measurement.outcome,
                    plan_digest = ?measurement.plan.as_ref().map(|plan| plan.digest),
                    "drained T4a shadow measurement"
                );
            }
        }
    }

    /// Closes the global lifecycle, requests active cancellation, and wakes every distinct domain.
    pub fn close(&self) {
        self.lifecycle.close();
        self.shadow.close();
        if let Some((_, token)) =
            self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref()
        {
            token.request_cancel();
        }
        self.shutdown.cancel();
        self.slot_notify.notify_waiters();
        self.control_notify.notify_waiters();
        self.set_a1_status(A1Status::Closed);
    }

    fn inspect_watchdog(&self) {
        let active =
            self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().cloned();
        let terminal = self
            .active_terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned();
        let (Some((generation, token)), Some((terminal_generation, terminal))) = (active, terminal)
        else {
            return;
        };
        if generation != terminal_generation {
            return;
        }
        match self.watchdog.inspect(Instant::now(), &token, &self.lifecycle) {
            WatchdogStatus::HungDisabled => {
                self.claim_terminal(
                    generation,
                    &terminal,
                    A1Outcome::InternalFailure,
                    TERMINAL_INTERNAL_FAILURE,
                    Some(ShadowOutcome::InternalFailure),
                );
                token.request_cancel();
                self.set_a1_status(A1Status::DisabledPermanent);
                self.shutdown.cancel();
                self.slot_notify.notify_waiters();
                self.control_notify.notify_waiters();
            }
            WatchdogStatus::Dropped => {
                self.claim_terminal(
                    generation,
                    &terminal,
                    A1Outcome::Cancelled,
                    TERMINAL_CANCELLED,
                    Some(ShadowOutcome::Cancelled),
                );
            }
            WatchdogStatus::Active | WatchdogStatus::Grace | WatchdogStatus::Completed => {}
        }
    }

    fn cancel_and_claim(&self, generation: u64, token: &CancellationToken, terminal: &AtomicU8) {
        token.request_cancel();
        let cancelled = match token.state() {
            TaskState::CancelRequested => token.acknowledge_drop(),
            TaskState::DroppedAcked => true,
            TaskState::Active | TaskState::Completed => false,
        };
        if cancelled {
            self.claim_terminal(
                generation,
                terminal,
                A1Outcome::Cancelled,
                TERMINAL_CANCELLED,
                Some(ShadowOutcome::Cancelled),
            );
        }
    }

    fn claim_terminal(
        &self,
        _generation: u64,
        terminal: &AtomicU8,
        outcome: A1Outcome,
        terminal_value: u8,
        shadow_outcome: Option<ShadowOutcome>,
    ) -> bool {
        if terminal
            .compare_exchange(
                TERMINAL_UNCLAIMED,
                terminal_value,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return false;
        }
        self.record_a1(outcome);
        if self.universe.is_some()
            && let Some(shadow_outcome) = shadow_outcome
        {
            self.shadow_outcomes.record(shadow_outcome);
        }
        true
    }

    fn clear_active(
        &self,
        generation: u64,
        token: &Arc<CancellationToken>,
        terminal: &Arc<AtomicU8>,
    ) {
        let mut active = self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.as_ref().is_some_and(|(active_generation, active_token)| {
            *active_generation == generation && Arc::ptr_eq(active_token, token)
        }) {
            *active = None;
        }
        drop(active);
        let mut active_terminal =
            self.active_terminal.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if active_terminal.as_ref().is_some_and(|(active_generation, active_guard)| {
            *active_generation == generation && Arc::ptr_eq(active_guard, terminal)
        }) {
            *active_terminal = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap, sync::Arc};

    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::{Address, B256, U256};
    use reth_provider::StateProviderBox;

    use super::*;
    use crate::{
        AuditedWriteKey, DescriptorHasher, DescriptorPlanDigest, ExactProtocol, FieldKind,
        FieldRead, FixturePoolRegistry, MeasurementEncoder, PreparedPoolQuote, PreparedPoolState,
        RegistryHasher, SnapshotHandleFactory, StorageReadPlan, WETH,
    };

    #[derive(Debug)]
    struct EmptyPort;

    impl TraderSnapshotPort for EmptyPort {
        fn capture_latest(
            &self,
            _factory: &SnapshotHandleFactory,
        ) -> Result<Option<crate::SnapshotHandle>, PortError> {
            Ok(None)
        }

        fn is_current_authoritative(&self, _handle: &crate::SnapshotHandle) -> bool {
            false
        }

        fn state_at_hash(&self, _block_hash: B256) -> Result<StateProviderBox, PortError> {
            Err(PortError::ProviderUnavailable)
        }

        fn sealed_header_at_hash(&self, _block_hash: B256) -> Result<Sealed<Header>, PortError> {
            Err(PortError::HeaderUnavailable)
        }
    }

    fn victim_at(received_at: Instant) -> BlinkVictim {
        BlinkVictim::decode(
            r#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"sub","timestamp":1,"publishTime":2,"blockNumber":"0x64","flashblockIndex":"0x2","result":{"chainId":"0xd","type":"0x2","hash":"0x0000000000000000000000000000000000000000000000000000000000000001","from":"0x0000000000000000000000000000000000000002","rawTx":"0x01"}}}"#,
            "sub",
            received_at,
        )
        .expect("victim")
    }

    fn victim() -> BlinkVictim {
        victim_at(Instant::now())
    }

    fn runtime() -> MevTraderRuntime {
        MevTraderRuntime::start(MevTraderRuntimeConfig::empty().expect("empty config"))
            .expect("runtime")
    }

    fn shadow_runtime() -> MevTraderRuntime {
        let mut descriptor = code_descriptor(1, ExactProtocol::UniswapV2);
        descriptor.audited_writes = [0u64, 1]
            .into_iter()
            .map(|slot| AuditedWriteKey::Storage {
                address: descriptor.pool,
                slot: U256::from(slot),
                evidence_digest: B256::with_last_byte(slot as u8 + 1),
            })
            .collect();
        descriptor.descriptor_digest =
            DescriptorHasher::digest(&descriptor).expect("descriptor digest");
        let descriptors = vec![descriptor];
        let digest = RegistryHasher::digest(&descriptors).expect("registry digest");
        let registry = FixturePoolRegistry::new(descriptors, digest).expect("registry");
        let snapshot = PoolUniverseSnapshot::capture(&registry).expect("snapshot");
        MevTraderRuntime::start(
            MevTraderRuntimeConfig::shadow(snapshot).expect("shadow runtime config"),
        )
        .expect("shadow runtime")
    }

    fn shadow_outcome_total(runtime: &MevTraderRuntime) -> u64 {
        [
            ShadowOutcome::FrameRejected,
            ShadowOutcome::CodeHashRejected,
            ShadowOutcome::PreparationRejected,
            ShadowOutcome::V3Coverage,
            ShadowOutcome::AnalysisRejected,
            ShadowOutcome::InternalFailure,
            ShadowOutcome::Cancelled,
            ShadowOutcome::NoDirtyPools,
            ShadowOutcome::NoCandidate,
            ShadowOutcome::NoPositivePlan,
            ShadowOutcome::Selected,
        ]
        .into_iter()
        .map(|outcome| runtime.shadow_outcome_counters().count(outcome))
        .sum()
    }

    fn code_descriptor(pool_byte: u8, protocol: ExactProtocol) -> PoolDescriptor {
        let field = |kind, slot| FieldRead {
            kind,
            slot: U256::from(slot),
            bit_offset: 0,
            bit_width: 112,
            signed: false,
        };
        PoolDescriptor {
            pool: Address::with_last_byte(pool_byte),
            protocol,
            token0: Address::with_last_byte(20),
            token1: Address::with_last_byte(21),
            decimals0: 6,
            decimals1: 18,
            fee: 3_000,
            code_hash: B256::with_last_byte(pool_byte.saturating_add(32)),
            read_plan: StorageReadPlan::constant_product(
                field(FieldKind::Reserve0, 0),
                field(FieldKind::Reserve1, 1),
            ),
            audited_writes: Vec::new(),
            descriptor_digest: DescriptorPlanDigest(B256::ZERO),
        }
    }

    #[test]
    fn t4a_code_hash_view_requires_all_pools_at_one_parent() {
        let descriptors = [
            ExactProtocol::UniswapV2,
            ExactProtocol::AerodromeVolatile,
            ExactProtocol::AerodromeStable,
            ExactProtocol::UniswapV3,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, protocol)| code_descriptor(index as u8 + 1, protocol))
        .collect::<Vec<_>>();
        let parent_hash = B256::with_last_byte(99);
        let observed = descriptors
            .iter()
            .map(|descriptor| (descriptor.pool, descriptor.code_hash))
            .collect::<BTreeMap<_, _>>();
        let mut read_parents = Vec::new();

        assert!(PoolCodeHashView::validate_at_parent(
            parent_hash,
            &descriptors,
            || true,
            |parent, pool| {
                read_parents.push(parent);
                Ok(observed.get(&pool).copied())
            },
        ));
        assert_eq!(read_parents, vec![parent_hash; descriptors.len()]);

        let mut missing = observed.clone();
        missing.remove(&descriptors[0].pool);
        assert!(!PoolCodeHashView::validate_at_parent(
            parent_hash,
            &descriptors,
            || true,
            |_, pool| Ok(missing.get(&pool).copied()),
        ));

        let mut mismatched = observed.clone();
        mismatched.insert(descriptors[1].pool, B256::with_last_byte(200));
        assert!(!PoolCodeHashView::validate_at_parent(
            parent_hash,
            &descriptors,
            || true,
            |_, pool| Ok(mismatched.get(&pool).copied()),
        ));

        assert!(!PoolCodeHashView::validate_at_parent(
            parent_hash,
            &descriptors,
            || true,
            |_, _| Err(PortError::ProviderUnavailable),
        ));

        let authority_checks = Cell::new(0usize);
        assert!(!PoolCodeHashView::validate_at_parent(
            parent_hash,
            &descriptors,
            || {
                let check = authority_checks.get();
                authority_checks.set(check + 1);
                check < descriptors.len()
            },
            |_, pool| Ok(observed.get(&pool).copied()),
        ));
    }

    #[test]
    fn latest_wins_and_taken_generation_has_one_terminal_outcome() {
        let runtime = runtime();
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        runtime.submit_blink_victim(victim());
        runtime.submit_blink_victim(victim());
        assert_eq!(runtime.counters().count(A1Outcome::SlotAccepted), 1);
        assert_eq!(runtime.counters().count(A1Outcome::SlotReplaced), 1);
        assert!(runtime.consume_once(&EmptyPort, Arc::clone(&chain_spec)));
        assert!(!runtime.consume_once(&EmptyPort, chain_spec));
        assert_eq!(runtime.counters().count(A1Outcome::NoTrade), 1);
        assert_eq!(runtime.counters().count(A1Outcome::Cancelled), 0);
        assert_eq!(runtime.counters().count(A1Outcome::InternalFailure), 0);
    }

    #[test]
    fn t4a_pairwise_pipeline_emits_zero_or_one_frame_bound_measurement_plan() {
        let processed = crate::frame::test_utils::processed_frame();
        let probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(1))),
            Arc::new(GlobalLifecycle::default()),
        );
        let token = Address::with_last_byte(0xaa);
        let pools = vec![
            PreparedPoolState {
                pool: Address::with_last_byte(1),
                protocol: ExactProtocol::UniswapV2,
                token0: WETH,
                token1: token,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::constant_product(
                    U256::from(1_000_000_000_000_000_000u64),
                    U256::from(1_000_000_000_000_000_000u64),
                ),
            },
            PreparedPoolState {
                pool: Address::with_last_byte(2),
                protocol: ExactProtocol::UniswapV2,
                token0: WETH,
                token1: token,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 500,
                quote: PreparedPoolQuote::constant_product(
                    U256::from(2_000_000_000_000_000_000u64),
                    U256::from(1_000_000_000_000_000_000u64),
                ),
            },
        ];
        let candidates = PairwiseEngine::discover("t4a-runtime", &pools, &[pools[0].pool], &probe)
            .expect("pairwise discovery");
        assert!(!candidates.is_empty());
        let plan = PairwiseEngine::select_measurement(&processed, &candidates, &probe)
            .expect("measurement selection")
            .expect("one positive plan");
        assert_eq!(plan.parent_hash, processed.measurement_context().parent_hash);
        assert_eq!(plan.block_number, processed.measurement_context().block_number);
        assert_eq!(plan.predecessor_index, processed.measurement_context().predecessor_index);
        assert_eq!(plan.payload_id, processed.measurement_context().payload_id);
        assert_eq!(plan.victim, processed.measurement_context().victim);
        assert!(!plan.gross_profit.is_zero());
        MeasurementEncoder::validate(&plan).expect("measurement digest");
    }

    #[test]
    fn terminal_status_never_reopens_and_close_rejects_slot() {
        let runtime = runtime();
        assert!(runtime.registry_is_empty());
        assert_eq!(runtime.analysis_thread_count(), crate::ANALYSIS_THREADS);
        assert!(runtime.worker_is_claimed());
        runtime.set_a1_status(A1Status::Subscribed);
        runtime.set_a1_status(A1Status::DisabledPermanent);
        runtime.set_a1_status(A1Status::Connecting);
        assert_eq!(runtime.a1_status(), A1Status::DisabledPermanent);
        runtime.close();
        runtime.submit_blink_victim(victim());
        assert_eq!(runtime.counters().count(A1Outcome::SlotClosed), 1);
    }

    #[test]
    fn t4a_stale_cancelled_or_head_changed_frame_emits_no_plan() {
        let runtime = shadow_runtime();
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        runtime.submit_blink_victim(victim_at(
            Instant::now() - Duration::from_millis(crate::DEADLINE_MILLIS + 1),
        ));
        assert!(runtime.consume_once(&EmptyPort, chain_spec));
        assert_eq!(runtime.counters().count(A1Outcome::Cancelled), 1);
        assert_eq!(runtime.counters().count(A1Outcome::NoTrade), 0);
        assert_eq!(runtime.counters().count(A1Outcome::InternalFailure), 0);
        assert_eq!(runtime.shadow_outcome_counters().count(ShadowOutcome::Cancelled), 1);
        assert_eq!(runtime.shadow_outcome_counters().count(ShadowOutcome::FrameRejected), 0);
        assert_eq!(shadow_outcome_total(&runtime), 1);

        let capture_rejected = shadow_runtime();
        capture_rejected.submit_blink_victim(victim_at(Instant::now() + Duration::from_secs(1)));
        assert!(capture_rejected.consume_once(&EmptyPort, Arc::new(BaseChainSpec::mainnet())));
        assert_eq!(
            capture_rejected.shadow_outcome_counters().count(ShadowOutcome::FrameRejected),
            1
        );
        assert_eq!(capture_rejected.shadow_outcome_counters().count(ShadowOutcome::Cancelled), 0);
        assert_eq!(shadow_outcome_total(&capture_rejected), 1);

        let preacknowledged = shadow_runtime();
        let preacknowledged_token = CancellationToken::new(Instant::now() + Duration::from_secs(1));
        assert!(preacknowledged_token.request_cancel());
        assert!(preacknowledged_token.acknowledge_drop());
        let preacknowledged_terminal = AtomicU8::new(TERMINAL_UNCLAIMED);
        preacknowledged.cancel_and_claim(1, &preacknowledged_token, &preacknowledged_terminal);
        assert_eq!(preacknowledged.counters().count(A1Outcome::Cancelled), 1);
        assert_eq!(preacknowledged.shadow_outcome_counters().count(ShadowOutcome::Cancelled), 1);
        assert_eq!(shadow_outcome_total(&preacknowledged), 1);

        let processed = crate::frame::test_utils::processed_frame();
        let expired_probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() - Duration::from_millis(1))),
            Arc::new(GlobalLifecycle::default()),
        );
        assert_eq!(
            PairwiseEngine::select_measurement(&processed, &[], &expired_probe),
            Err(PairwiseError::Cancelled)
        );

        let cancelled_token =
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(1)));
        cancelled_token.request_cancel();
        let cancelled_probe =
            CancellationProbe::new(cancelled_token, Arc::new(GlobalLifecycle::default()));
        assert_eq!(
            PairwiseEngine::select_measurement(&processed, &[], &cancelled_probe),
            Err(PairwiseError::Cancelled)
        );

        let head_changed = crate::frame::test_utils::TestFrameHarness::capture();
        head_changed.allow_authority_checks(0);
        let head_probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(1))),
            Arc::new(GlobalLifecycle::default()),
        );
        assert!(matches!(
            head_changed.process_result(&head_changed.audited_writes(), &head_probe),
            Ok(None)
        ));
    }

    #[test]
    fn watchdog_and_late_return_share_one_terminal_guard() {
        let runtime = shadow_runtime();
        let generation = 1;
        let deadline = Instant::now() - Duration::from_millis(crate::HANG_GRACE_MILLIS + 1);
        let token = Arc::new(CancellationToken::new(deadline));
        let terminal = Arc::new(AtomicU8::new(TERMINAL_UNCLAIMED));
        *runtime.active.lock().expect("active") = Some((generation, Arc::clone(&token)));
        *runtime.active_terminal.lock().expect("terminal") =
            Some((generation, Arc::clone(&terminal)));

        runtime.inspect_watchdog();
        runtime.inspect_watchdog();
        runtime.claim_terminal(
            generation,
            &terminal,
            A1Outcome::InternalFailure,
            TERMINAL_INTERNAL_FAILURE,
            Some(ShadowOutcome::Cancelled),
        );
        assert_eq!(runtime.counters().count(A1Outcome::InternalFailure), 1);
        assert_eq!(runtime.counters().count(A1Outcome::Cancelled), 0);
        assert_eq!(runtime.shadow_outcome_counters().count(ShadowOutcome::InternalFailure), 1);
        assert_eq!(runtime.shadow_outcome_counters().count(ShadowOutcome::Cancelled), 0);
        assert_eq!(shadow_outcome_total(&runtime), 1);
        assert!(runtime.shutdown().is_cancelled());
    }

    #[test]
    fn generation_overflow_is_terminal_and_root_cancelled() {
        let runtime = runtime();
        runtime.generation.store(u64::MAX, Ordering::SeqCst);
        runtime.submit_blink_victim(victim());
        assert_eq!(runtime.counters().count(A1Outcome::GenerationOverflow), 1);
        assert_eq!(runtime.a1_status(), A1Status::DisabledPermanent);
        assert!(runtime.shutdown().is_cancelled());
    }
}
