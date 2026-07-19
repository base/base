//! Receive-only A1 runtime ownership, latest-wins isolation, and terminal accounting.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_execution_chainspec::BaseChainSpec;
use thiserror::Error;
use tokio::{sync::Notify, time};

use crate::{
    A1Counters, A1Outcome, A1Status, BlinkVictim, CancellationProbe, CancellationToken,
    DedicatedAnalysisPool, FixturePoolRegistry, FrameProcessor, GlobalLifecycle, GlobalState,
    LatestSlot, LifecycleError, PortError, QueuedBlinkVictim, RegistryError, RegistryHasher,
    RuntimeShutdown, SlotSubmit, SnapshotCaptureCoordinator, SoleWorker, TaskRun, TaskRunner,
    TaskState, TraderSnapshotPort, VictimFrame, Watchdog, WatchdogStatus, WorkerClaim,
};

const TERMINAL_UNCLAIMED: u8 = 0;
const TERMINAL_FRAME_BOUND: u8 = 1;
const TERMINAL_NO_TRADE: u8 = 2;
const TERMINAL_CANCELLED: u8 = 3;
const TERMINAL_INTERNAL_FAILURE: u8 = 4;
const CONTROL_INTERVAL: Duration = Duration::from_millis(5);

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
}

impl MevTraderRuntimeConfig {
    /// Constructs and validates the canonical empty fixture registry.
    pub fn empty() -> Result<Self, RuntimeInstallError> {
        let descriptors = Vec::new();
        let digest = RegistryHasher::digest(&descriptors)?;
        let registry = FixturePoolRegistry::new(descriptors, digest)?;
        Ok(Self { registry })
    }

    /// Returns true because A1 production wiring accepts only an empty registry.
    pub const fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }
}

/// Sole receive-only runtime owner for ingress, consumer, control, and Rayon4 state.
#[derive(Debug)]
pub struct MevTraderRuntime {
    registry: FixturePoolRegistry,
    lifecycle: Arc<GlobalLifecycle>,
    ingress: Arc<LatestSlot<QueuedBlinkVictim>>,
    analysis: DedicatedAnalysisPool,
    worker: WorkerClaim,
    watchdog: Watchdog,
    counters: Arc<A1Counters>,
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
            lifecycle,
            ingress,
            analysis,
            worker,
            watchdog: Watchdog,
            counters: Arc::new(A1Counters::default()),
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
            match self.status.compare_exchange(current, next as u8, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns the fixed counters shared by ingress and runtime control.
    pub const fn counters(&self) -> &Arc<A1Counters> {
        &self.counters
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
        let generation = match self.generation.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |generation| generation.checked_add(1),
        ) {
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

        if let Some((active_generation, token)) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
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
        *self
            .active_terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
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
            Ok(None) | Err(PortError::SnapshotUnavailable)
            | Err(PortError::ProviderUnavailable)
            | Err(PortError::HeaderUnavailable)
            | Err(PortError::VisitorStopped)
            | Err(PortError::LimitExceeded)
            | Err(PortError::Incoherent)
            | Err(PortError::FactoryAlreadyUsed) => {
                self.complete_or_cancel(
                    generation,
                    &token,
                    &terminal,
                    A1Outcome::NoTrade,
                    TERMINAL_NO_TRADE,
                    true,
                );
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
                FrameProcessor::process(port, &snapshot, &frame, Instant::now(), chain_spec, &[], probe)
            })
        });
        match run {
            TaskRun::Panicked => {
                self.claim_terminal(
                    generation,
                    &terminal,
                    A1Outcome::InternalFailure,
                    TERMINAL_INTERNAL_FAILURE,
                );
                token.request_cancel();
                self.set_a1_status(A1Status::DisabledPermanent);
                self.shutdown.cancel();
                self.slot_notify.notify_waiters();
                self.control_notify.notify_waiters();
            }
            TaskRun::Returned(None) => self.cancel_and_claim(generation, &token, &terminal),
            TaskRun::Returned(Some(Err(
                PortError::SnapshotUnavailable
                | PortError::ProviderUnavailable
                | PortError::HeaderUnavailable
                | PortError::VisitorStopped
                | PortError::LimitExceeded
                | PortError::Incoherent
                | PortError::FactoryAlreadyUsed,
            )))
            | TaskRun::Returned(Some(Ok(None))) => {
                let current = self.generation.load(Ordering::SeqCst) == generation
                    && port.is_current_authoritative(&snapshot);
                self.complete_or_cancel(
                    generation,
                    &token,
                    &terminal,
                    A1Outcome::NoTrade,
                    TERMINAL_NO_TRADE,
                    current,
                );
            }
            TaskRun::Returned(Some(Ok(Some(_materialized)))) => {
                let current = self.generation.load(Ordering::SeqCst) == generation
                    && port.is_current_authoritative(&snapshot);
                self.complete_or_cancel(
                    generation,
                    &token,
                    &terminal,
                    A1Outcome::FrameBound,
                    TERMINAL_FRAME_BOUND,
                    current,
                );
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
        }
    }

    /// Closes the global lifecycle, requests active cancellation, and wakes every distinct domain.
    pub fn close(&self) {
        self.lifecycle.close();
        if let Some((_, token)) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            token.request_cancel();
        }
        self.shutdown.cancel();
        self.slot_notify.notify_waiters();
        self.control_notify.notify_waiters();
        self.set_a1_status(A1Status::Closed);
    }

    fn inspect_watchdog(&self) {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned();
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
                );
            }
            WatchdogStatus::Active | WatchdogStatus::Grace | WatchdogStatus::Completed => {}
        }
    }

    fn complete_or_cancel(
        &self,
        generation: u64,
        token: &CancellationToken,
        terminal: &AtomicU8,
        outcome: A1Outcome,
        terminal_value: u8,
        current: bool,
    ) {
        if token.complete(Instant::now(), current, &self.lifecycle) {
            self.claim_terminal(generation, terminal, outcome, terminal_value);
        } else {
            self.cancel_and_claim(generation, token, terminal);
        }
    }

    fn cancel_and_claim(
        &self,
        generation: u64,
        token: &CancellationToken,
        terminal: &AtomicU8,
    ) {
        token.request_cancel();
        if token.state() == TaskState::CancelRequested && token.acknowledge_drop() {
            self.claim_terminal(
                generation,
                terminal,
                A1Outcome::Cancelled,
                TERMINAL_CANCELLED,
            );
        }
    }

    fn claim_terminal(
        &self,
        _generation: u64,
        terminal: &AtomicU8,
        outcome: A1Outcome,
        terminal_value: u8,
    ) {
        if terminal
            .compare_exchange(
                TERMINAL_UNCLAIMED,
                terminal_value,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.record_a1(outcome);
        }
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
        let mut active_terminal = self
            .active_terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active_terminal.as_ref().is_some_and(|(active_generation, active_guard)| {
            *active_generation == generation && Arc::ptr_eq(active_guard, terminal)
        }) {
            *active_terminal = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::B256;
    use reth_provider::StateProviderBox;

    use super::*;
    use crate::SnapshotHandleFactory;

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
    fn expired_generation_acknowledges_cancel_before_counting() {
        let runtime = runtime();
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        runtime.submit_blink_victim(victim_at(
            Instant::now() - Duration::from_millis(crate::DEADLINE_MILLIS + 1),
        ));
        assert!(runtime.consume_once(&EmptyPort, chain_spec));
        assert_eq!(runtime.counters().count(A1Outcome::Cancelled), 1);
        assert_eq!(runtime.counters().count(A1Outcome::NoTrade), 0);
        assert_eq!(runtime.counters().count(A1Outcome::InternalFailure), 0);
    }

    #[test]
    fn watchdog_and_late_return_share_one_terminal_guard() {
        let runtime = runtime();
        let generation = 1;
        let deadline =
            Instant::now() - Duration::from_millis(crate::HANG_GRACE_MILLIS + 1);
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
        );
        assert_eq!(runtime.counters().count(A1Outcome::InternalFailure), 1);
        assert_eq!(runtime.counters().count(A1Outcome::Cancelled), 0);
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
