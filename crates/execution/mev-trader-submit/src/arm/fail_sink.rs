//! Shared durable kill-state observer and process-local fail-stop poison sink.

#[cfg(test)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base_mev_trader::{
    AnchoredKillStateStore, KillReason, KillState, KillStateStore, StartupError,
};

use super::ArmError;
#[cfg(not(test))]
static PROCESS_POISON: AtomicBool = AtomicBool::new(false);

/// Stable result of attempting the mandatory production fail-stop latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionLatchOutcome {
    /// The durable kill latch was engaged.
    Engaged,
    /// Durable persistence failed, while process poison remains set.
    PersistFailed,
    /// Process poison was already active before this operation.
    AlreadyPoisoned,
}

#[derive(Debug)]
struct ProcessPoison(#[cfg(not(test))] &'static AtomicBool, #[cfg(test)] Arc<AtomicBool>);

impl ProcessPoison {
    fn for_constructor() -> Self {
        #[cfg(not(test))]
        {
            Self(&PROCESS_POISON)
        }
        #[cfg(test)]
        {
            Self(Arc::new(AtomicBool::new(false)))
        }
    }

    #[cfg(test)]
    fn shared(state: Arc<AtomicBool>) -> Self {
        Self(state)
    }

    fn load(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn fetch_or(&self, value: bool) -> bool {
        self.0.fetch_or(value, Ordering::SeqCst)
    }

    fn poison(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The single shared fail-stop sink: a durable [`KillStateStore`] handle plus a
/// process-local poison handle. In production every instance points to one
/// process-lifetime static; test constructors receive isolated handles so parallel
/// tests cannot poison one another. Once poisoned, every entrypoint refuses.
///
/// [`latch`](Self::latch) engages the durable kill AND sets the poison flag on
/// BOTH the success and failure of the durable `engage` — a signing/claim failure
/// is fail-stop regardless of whether the latch persisted.
pub struct ArmedFailSink {
    kill: Box<dyn KillStateStore + Send + Sync>,
    process_poison: ProcessPoison,
}

impl core::fmt::Debug for ArmedFailSink {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArmedFailSink")
            .field("poisoned", &self.process_poison.load())
            .finish_non_exhaustive()
    }
}

impl ArmedFailSink {
    /// Builds the production sink from the sole pinned, anchor-backed store.
    ///
    /// Construction refuses without returning a sink unless the first durable observation is
    /// verified clear.
    pub fn from_anchored(kill: AnchoredKillStateStore) -> Result<Self, StartupError> {
        Self::from_store_checked(Box::new(kill), ProcessPoison::for_constructor())
    }

    /// Test-only generic injection for mutable failure stores.
    #[cfg(test)]
    pub(crate) fn new(kill: Box<dyn KillStateStore + Send + Sync>) -> Result<Self, StartupError> {
        Self::from_store_checked(kill, ProcessPoison::for_constructor())
    }

    #[cfg(test)]
    fn new_with_process_poison(
        kill: Box<dyn KillStateStore + Send + Sync>,
        process_poison: ProcessPoison,
    ) -> Result<Self, StartupError> {
        Self::from_store_checked(kill, process_poison)
    }

    fn from_store_checked(
        kill: Box<dyn KillStateStore + Send + Sync>,
        process_poison: ProcessPoison,
    ) -> Result<Self, StartupError> {
        let sink = Self { kill, process_poison };
        sink.observe_kill().map_err(|_| StartupError::KillStateNotClear)?;
        Ok(sink)
    }

    /// Whether this process has been poisoned by a prior fail-stop latch.
    pub fn is_poisoned(&self) -> bool {
        self.process_poison.load()
    }

    /// Performs the single authoritative durable kill observation.
    ///
    /// Once poison is set, no later durable state can clear it. A fresh non-clear observation
    /// poisons before refusing.
    pub fn observe_kill(&self) -> Result<KillState, ArmError> {
        if self.is_poisoned() {
            return Err(ArmError::Poisoned);
        }
        let state = self.kill.load();
        let is_clear = matches!(state, KillState::Clear { .. });
        // One SeqCst RMW linearizes a Clear success against every concurrent poison. A Clear
        // observer refuses when poison was already set; a non-Clear observer sets poison before
        // refusing, and no later observation can clear it.
        let was_poisoned = self.process_poison.fetch_or(!is_clear);
        if was_poisoned || !is_clear { Err(ArmError::Poisoned) } else { Ok(state) }
    }

    /// Re-read the durable kill state and fail closed unless it is verified clear.
    pub fn check(&self) -> Result<(), ArmError> {
        self.observe_kill().map(|_| ())
    }

    /// Fail-stop latch: engage the durable kill with `reason` and poison the
    /// process. The poison is set whether or not the durable engage succeeds, so a
    /// persistence failure does NOT weaken the fail-stop. Returns the corresponding
    /// [`ArmError`] the caller should surface.
    pub fn latch(&self, reason: KillReason) -> ArmError {
        // Poison FIRST (SeqCst): during a durable-engage fsync delay or failure no
        // other thread may pass `check()`. The poison is unconditional (fail-stop),
        // so a failed durable engage does not weaken the halt.
        self.process_poison.poison();
        let engaged = self.kill.engage(reason);
        match engaged {
            Ok(()) => ArmError::KillReason(reason),
            Err(_) => ArmError::LatchPersistFailed,
        }
    }

    /// Attempts the key/signature fail-stop latch and preserves its exact stable outcome.
    pub fn latch_production(&self) -> ProductionLatchOutcome {
        if self.is_poisoned() {
            return ProductionLatchOutcome::AlreadyPoisoned;
        }
        match self.latch(KillReason::KeyOrSignatureFailure) {
            ArmError::KillReason(KillReason::KeyOrSignatureFailure) => {
                ProductionLatchOutcome::Engaged
            }
            ArmError::LatchPersistFailed => ProductionLatchOutcome::PersistFailed,
            ArmError::Poisoned => ProductionLatchOutcome::AlreadyPoisoned,
            ArmError::KillReason(
                KillReason::StrictMinOutPrincipalLoss | KillReason::DrawdownFloorBreach,
            )
            | ArmError::Freshness
            | ArmError::AlreadyClaimed => unreachable!("fixed production latch reason"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use base_mev_trader::{
        KillReason, KillState, KillStateStore, KillStoreError, ResetAttestation, StartupError,
    };

    use super::{ArmedFailSink, ProcessPoison};
    use crate::arm::ArmError;

    #[derive(Clone)]
    struct PausedClearStore {
        state: Arc<Mutex<KillState>>,
        fail_engage: bool,
        pause_load: Arc<AtomicBool>,
        load_entered: Arc<Barrier>,
        release_load: Arc<Barrier>,
    }

    impl PausedClearStore {
        fn new(fail_engage: bool) -> Self {
            Self {
                state: Arc::new(Mutex::new(KillState::Clear { verified_at: 0 })),
                fail_engage,
                pause_load: Arc::new(AtomicBool::new(false)),
                load_entered: Arc::new(Barrier::new(2)),
                release_load: Arc::new(Barrier::new(2)),
            }
        }

        fn set_clear(&self) {
            *self.state.lock().expect("paused store lock") = KillState::Clear { verified_at: 1 };
        }
    }

    impl KillStateStore for PausedClearStore {
        fn load(&self) -> KillState {
            let state = *self.state.lock().expect("paused store lock");
            if self.pause_load.load(Ordering::SeqCst) {
                self.load_entered.wait();
                self.release_load.wait();
            }
            state
        }

        fn engage(&self, reason: KillReason) -> Result<(), KillStoreError> {
            if self.fail_engage {
                return Err(KillStoreError::Io);
            }
            *self.state.lock().expect("paused store lock") = KillState::Engaged { reason };
            Ok(())
        }

        fn owner_reset(&self, _attestation: &ResetAttestation) -> Result<(), KillStoreError> {
            Err(KillStoreError::OwnerSignatureMismatch)
        }
    }

    fn clear_observer_loses_to_concurrent_latch(fail_engage: bool) {
        let store = PausedClearStore::new(fail_engage);
        let sink = Arc::new(ArmedFailSink::new(Box::new(store.clone())).expect("clear startup"));
        store.pause_load.store(true, Ordering::SeqCst);

        let observer_sink = Arc::clone(&sink);
        let observer = std::thread::spawn(move || observer_sink.observe_kill());
        store.load_entered.wait();

        let latch = sink.latch(KillReason::KeyOrSignatureFailure);
        if fail_engage {
            assert!(matches!(latch, ArmError::LatchPersistFailed));
        } else {
            assert!(matches!(latch, ArmError::KillReason(KillReason::KeyOrSignatureFailure)));
        }

        store.release_load.wait();
        assert!(matches!(observer.join().expect("observer thread"), Err(ArmError::Poisoned)));
        store.set_clear();
        assert!(sink.is_poisoned());
        assert!(matches!(sink.observe_kill(), Err(ArmError::Poisoned)));
    }

    #[test]
    fn poison_survives_drop_and_reconstruction_after_persist_failure() {
        let store = PausedClearStore::new(true);
        let poison_state = Arc::new(AtomicBool::new(false));

        let first = ArmedFailSink::new_with_process_poison(
            Box::new(store.clone()),
            ProcessPoison::shared(Arc::clone(&poison_state)),
        )
        .expect("clear startup");
        assert!(matches!(
            first.latch(KillReason::KeyOrSignatureFailure),
            ArmError::LatchPersistFailed
        ));
        assert!(matches!(store.load(), KillState::Clear { .. }));
        drop(first);

        let reconstruction = ArmedFailSink::new_with_process_poison(
            Box::new(store.clone()),
            ProcessPoison::shared(Arc::clone(&poison_state)),
        );
        assert!(matches!(reconstruction, Err(StartupError::KillStateNotClear)));
        assert!(poison_state.load(Ordering::SeqCst));

        store.set_clear();
        let repeated = ArmedFailSink::new_with_process_poison(
            Box::new(store),
            ProcessPoison::shared(poison_state),
        );
        assert!(matches!(repeated, Err(StartupError::KillStateNotClear)));
    }

    #[test]
    fn clear_observer_loses_when_concurrent_latch_engage_succeeds() {
        clear_observer_loses_to_concurrent_latch(false);
    }

    #[test]
    fn clear_observer_loses_when_concurrent_latch_engage_fails() {
        clear_observer_loses_to_concurrent_latch(true);
    }
}
