//! High-level simulation-only submission entrypoint and typed unavailable sink.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread::JoinHandle,
};

#[cfg(feature = "t4e-handoff")]
use crate::{SealedUnsignedCandidate, T4eCandidateHandoff, T4eHandoffError};

use super::{
    FreshnessSources, RuntimeBackend, SimBackend, SimulationPersistError, SimulationPersisted,
    SimulationStore, SubmissionAttempt, SubmitOutcome, send_gated,
};

/// Missing production prerequisite. No variant supplies fallback data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationEntrypointUnavailable {
    /// Complete production T4e simulation installation is explicitly deferred.
    ProductionInstallationDeferred,
    /// Durable local ledger could not open.
    PersistenceUnavailable,
    /// Entrypoint status mutex was poisoned.
    StatusPoisoned,
    /// The named worker terminated unexpectedly. `panicked` is bounded panic evidence.
    WorkerClosed {
        /// Whether unwinding, rather than a typed terminal outcome, closed the worker.
        panicked: bool,
    },
}

/// Sticky operator-visible ledger closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationLedgerClosure {
    /// Fixed campaign bound was reached before irreversible work.
    Full {
        /// Epoch that filled.
        epoch: super::SimulationLedgerEpoch,
        /// Rejected sequence.
        next_sequence: u64,
        /// Fixed capacity.
        capacity: u64,
    },
    /// A durable publication operation failed.
    WriteFailed {
        /// Epoch being written.
        epoch: super::SimulationLedgerEpoch,
        /// Sequence with unknown durability.
        next_sequence: u64,
        /// Failed operation.
        operation: super::SimulationStoreOperation,
        /// Stable I/O class.
        kind: std::io::ErrorKind,
    },
    /// Record did not carry the sole checked economics receipt.
    MissingEconomics,
    /// Record lacked bounded T4e transaction/route evidence.
    MissingIdentityEvidence,
    /// Canonical encoding exceeded its fixed bound.
    Oversize,
    /// Existing ledger failed strict startup validation.
    InvalidExistingLedger {
        /// Epoch is unavailable when metadata itself is invalid.
        epoch: Option<super::SimulationLedgerEpoch>,
        /// Closed structural class.
        class: super::SimulationLedgerInvalid,
    },
}

impl From<SimulationPersistError> for SimulationLedgerClosure {
    fn from(error: SimulationPersistError) -> Self {
        match error {
            SimulationPersistError::Full { epoch, next_sequence, capacity } => {
                Self::Full { epoch, next_sequence, capacity }
            }
            SimulationPersistError::WriteFailed { epoch, next_sequence, operation, kind } => {
                Self::WriteFailed { epoch, next_sequence, operation, kind }
            }
            SimulationPersistError::MissingEconomics => Self::MissingEconomics,
            SimulationPersistError::MissingIdentityEvidence => Self::MissingIdentityEvidence,
            SimulationPersistError::Oversize => Self::Oversize,
        }
    }
}

/// Sticky process-lifetime entrypoint status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationEntrypointStatus {
    /// Store and simulation backend are accepting attempts.
    Ready,
    /// Required authority/runtime was unavailable at installation.
    Unavailable(SimulationEntrypointUnavailable),
    /// Ledger closed and requires the documented owner rotation/triage act.
    LedgerClosed(SimulationLedgerClosure),
}

impl SimulationEntrypointStatus {
    pub(crate) fn from_store_open_error(error: super::SimulationStoreOpenError) -> Self {
        match error {
            super::SimulationStoreOpenError::InvalidExistingLedger(class) => {
                Self::LedgerClosed(SimulationLedgerClosure::InvalidExistingLedger {
                    epoch: None,
                    class,
                })
            }
            super::SimulationStoreOpenError::Io(_)
            | super::SimulationStoreOpenError::Lease(_)
            | super::SimulationStoreOpenError::AlreadyOpen => {
                Self::Unavailable(SimulationEntrypointUnavailable::PersistenceUnavailable)
            }
        }
    }
}
/// Terminal classification for one accepted prepared submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationEntrypointTerminal {
    /// Attempt was fsync'd and atomically published.
    Persisted(SimulationPersisted),
    /// Egress-moment freshness refused the attempt.
    FreshnessClosed,
    /// Ledger closed; status contains the distinct operator reason.
    LedgerClosed(SimulationLedgerClosure),
    /// Simulation-only entrypoint observed a live-only outcome.
    UnexpectedLiveOutcome,
}

/// Library execution seam for `send_gated`, fixed to `SimBackend`.
///
/// This seam is not production-installed until `Production T4e Simulation Installation + Settled-Loss Authority` supplies real settled-loss authority, proofs/claim-store/custody, the shared bridge, and the PR #55 committed-state dependency. Production `Ready`, `Busy`, and `Closed` handoff behavior remains deferred to that complete installer.
#[derive(Debug)]
pub struct SimulationEntrypoint {
    backend: SimBackend,
    status: Mutex<SimulationEntrypointStatus>,
}

impl SimulationEntrypoint {
    /// Installs a ready simulation-only entrypoint.
    pub const fn ready() -> Self {
        Self { backend: SimBackend, status: Mutex::new(SimulationEntrypointStatus::Ready) }
    }

    /// Returns the sticky operator-visible status.
    pub fn status(&self) -> SimulationEntrypointStatus {
        self.status.lock().map(|status| *status).unwrap_or(SimulationEntrypointStatus::Unavailable(
            SimulationEntrypointUnavailable::StatusPoisoned,
        ))
    }

    /// Processes one admitted unified-path attempt on its owning OS worker.
    ///
    /// The caller is the dedicated bounded worker and owns the linear reservation spanning
    /// preparation through this synchronous terminal projection.
    pub(crate) fn process(
        &self,
        attempt: SubmissionAttempt,
        freshness: &FreshnessSources<'_>,
        store: &mut SimulationStore,
    ) -> SimulationEntrypointTerminal {
        if let Err(error) = store.ensure_capacity() {
            return self.close(error);
        }
        if self.status() != SimulationEntrypointStatus::Ready {
            return match self.status() {
                SimulationEntrypointStatus::LedgerClosed(reason) => {
                    SimulationEntrypointTerminal::LedgerClosed(reason)
                }
                SimulationEntrypointStatus::Ready | SimulationEntrypointStatus::Unavailable(_) => {
                    SimulationEntrypointTerminal::UnexpectedLiveOutcome
                }
            };
        }
        match send_gated(attempt, freshness, RuntimeBackend::simulated(&self.backend)) {
            SubmitOutcome::Simulated(record) => match store.append(&record) {
                Ok(persisted) => {
                    if let Err(error) = store.ensure_capacity() {
                        self.set_closed(error);
                    }
                    SimulationEntrypointTerminal::Persisted(persisted)
                }
                Err(error) => self.close(error),
            },
            SubmitOutcome::NoEgress => SimulationEntrypointTerminal::FreshnessClosed,
            SubmitOutcome::LiveLocksClosed(_)
            | SubmitOutcome::InclusionFailed(_)
            | SubmitOutcome::InclusionSentAttributionFailed { .. }
            | SubmitOutcome::LiveComplete => SimulationEntrypointTerminal::UnexpectedLiveOutcome,
        }
    }

    fn close(&self, error: SimulationPersistError) -> SimulationEntrypointTerminal {
        let reason = SimulationLedgerClosure::from(error);
        if let Ok(mut status) = self.status.lock() {
            *status = SimulationEntrypointStatus::LedgerClosed(reason);
        }
        SimulationEntrypointTerminal::LedgerClosed(reason)
    }

    fn set_closed(&self, error: SimulationPersistError) {
        let reason = SimulationLedgerClosure::from(error);
        if let Ok(mut status) = self.status.lock() {
            *status = SimulationEntrypointStatus::LedgerClosed(reason);
        }
    }

    fn close_worker(&self, admission: &AtomicU8, panicked: bool) {
        if let Ok(mut status) = self.status.lock() {
            *status = SimulationEntrypointStatus::Unavailable(
                SimulationEntrypointUnavailable::WorkerClosed { panicked },
            );
            admission.store(ADMISSION_CLOSED, Ordering::Release);
        } else {
            admission.store(ADMISSION_CLOSED, Ordering::Release);
        }
    }
}

/// Non-blocking capacity-one handoff into the sole simulation egress owner.
#[derive(Debug)]
pub(crate) struct SimulationWorker {
    sender: Option<SyncSender<AdmittedAttempt>>,
    admission: Arc<AtomicU8>,
    entrypoint: Arc<SimulationEntrypoint>,
    thread: Option<JoinHandle<()>>,
}

/// Linear, non-forgeable ownership of the sole worker/ledger admission slot.
///
/// Claiming and signing may start only after this value has been obtained. Dropping an
/// unused reservation returns admission; submitting it transfers that ownership to the worker.
#[derive(Debug)]
pub(crate) struct SimulationReservation {
    admission: Arc<AtomicU8>,
}

/// Typed pre-preparation admission refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimulationReservationError {
    Busy,
    Closed,
}

/// A post-preparation submission can fail only because the worker closed or its invariant broke;
/// queue fullness is deliberately not an exposed outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimulationSubmitError {
    Closed,
    AdmissionInvariant,
}

#[derive(Debug)]
struct AdmittedAttempt {
    attempt: SubmissionAttempt,
    _reservation: SimulationReservation,
}

const ADMISSION_FREE: u8 = 0;
const ADMISSION_OCCUPIED: u8 = 1;
const ADMISSION_CLOSED: u8 = 2;

impl Drop for SimulationReservation {
    fn drop(&mut self) {
        let _ = self.admission.compare_exchange(
            ADMISSION_OCCUPIED,
            ADMISSION_FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl SimulationWorker {
    /// Spawns the only thread permitted to call the synchronous simulation entrypoint.
    pub(crate) fn spawn<C, D>(
        runtime: super::ProductionB5Runtime<C, D>,
        armed: base_mev_trader::ArmedCriteria,
        mut store: SimulationStore,
    ) -> Result<Self, std::io::Error>
    where
        C: super::CommittedStateAuthority + Send + Sync + 'static,
        D: super::DrawdownAuthority + Send + Sync + 'static,
    {
        let (sender, receiver) = sync_channel(1);
        let admission = Arc::new(AtomicU8::new(ADMISSION_FREE));
        let entrypoint = Arc::new(SimulationEntrypoint::ready());
        if let Err(error) = store.ensure_capacity() {
            entrypoint.set_closed(error);
        }
        let worker_entrypoint = Arc::clone(&entrypoint);
        let worker_admission = Arc::clone(&admission);
        let thread = std::thread::Builder::new().name("base-mev-arm-egress".to_owned()).spawn(
            move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    while let Ok(admitted) = receiver.recv() {
                        let AdmittedAttempt { attempt, _reservation } = admitted;
                        let terminal = match catch_unwind(AssertUnwindSafe(|| {
                            let freshness = runtime.freshness(&armed);
                            worker_entrypoint.process(attempt, &freshness, &mut store)
                        })) {
                            Ok(terminal) => terminal,
                            Err(_) => {
                                worker_entrypoint.close_worker(&worker_admission, true);
                                drop(_reservation);
                                return;
                            }
                        };
                        if terminal == SimulationEntrypointTerminal::UnexpectedLiveOutcome {
                            worker_entrypoint.close_worker(&worker_admission, false);
                            drop(_reservation);
                            return;
                        }
                        drop(_reservation);
                        if worker_entrypoint.status() != SimulationEntrypointStatus::Ready
                            || matches!(terminal, SimulationEntrypointTerminal::LedgerClosed(_))
                        {
                            break;
                        }
                    }
                }));
                if outcome.is_err() {
                    worker_entrypoint.close_worker(&worker_admission, true);
                }
            },
        )?;
        Ok(Self { sender: Some(sender), admission, entrypoint, thread: Some(thread) })
    }

    /// Reserves the only running-or-queued slot before claim/signing starts.
    pub(crate) fn try_reserve(&self) -> Result<SimulationReservation, SimulationReservationError> {
        if self.sender.is_none() {
            return Err(SimulationReservationError::Closed);
        }
        let status =
            self.entrypoint.status.lock().map_err(|_| SimulationReservationError::Closed)?;
        if *status != SimulationEntrypointStatus::Ready {
            return Err(SimulationReservationError::Closed);
        }
        match self.admission.compare_exchange(
            ADMISSION_FREE,
            ADMISSION_OCCUPIED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(SimulationReservation { admission: Arc::clone(&self.admission) }),
            Err(ADMISSION_OCCUPIED) => Err(SimulationReservationError::Busy),
            Err(_) => Err(SimulationReservationError::Closed),
        }
    }

    /// Transfers an admitted, already-prepared attempt without blocking.
    ///
    /// Because the reservation spans both the running and queued states, `Full` is an internal
    /// invariant violation rather than an ordinary rejection of signed work.
    pub(crate) fn submit(
        &self,
        reservation: SimulationReservation,
        attempt: SubmissionAttempt,
    ) -> Result<(), SimulationSubmitError> {
        if !Arc::ptr_eq(&reservation.admission, &self.admission) {
            return Err(SimulationSubmitError::AdmissionInvariant);
        }
        let sender = self.sender.as_ref().ok_or(SimulationSubmitError::Closed)?;
        sender.try_send(AdmittedAttempt { attempt, _reservation: reservation }).map_err(|error| {
            match error {
                TrySendError::Full(_) => SimulationSubmitError::AdmissionInvariant,
                TrySendError::Disconnected(_) => SimulationSubmitError::Closed,
            }
        })
    }

    pub(crate) fn status(&self) -> SimulationEntrypointStatus {
        self.entrypoint.status()
    }
}

impl Drop for SimulationWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            self.entrypoint.close_worker(&self.admission, true);
        }
    }
}
/// Rejecting T4e sink installed when a production prerequisite is unavailable.
#[cfg(feature = "t4e-handoff")]
#[derive(Debug)]
pub struct UnavailableSimulationHandoff {
    status: SimulationEntrypointStatus,
}

#[cfg(feature = "t4e-handoff")]
impl UnavailableSimulationHandoff {
    /// Builds a typed rejecting sink; it owns no candidate storage.
    pub const fn new(reason: SimulationEntrypointUnavailable) -> Self {
        Self { status: SimulationEntrypointStatus::Unavailable(reason) }
    }

    /// Returns the exact typed installation failure.
    pub const fn status(&self) -> SimulationEntrypointStatus {
        self.status
    }

    /// Builds the explicitly deferred production sink without opening or probing any runtime.
    ///
    /// Production remains rejection-only until `Production T4e Simulation Installation + Settled-Loss Authority` installs the complete real authority path.
    pub const fn deferred_production() -> Self {
        Self::new(SimulationEntrypointUnavailable::ProductionInstallationDeferred)
    }
    /// Erases only the sink type, not its separately observable status.
    pub fn into_handoff(self) -> Arc<dyn T4eCandidateHandoff> {
        Arc::new(self)
    }
}

#[cfg(feature = "t4e-handoff")]
impl T4eCandidateHandoff for UnavailableSimulationHandoff {
    fn try_handoff(&self, _candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {
        Err(T4eHandoffError::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    fn campaign() -> base_mev_trader::CampaignId {
        base_mev_trader::CampaignId::new([0x0A; 32])
    }

    fn attempt() -> SubmissionAttempt {
        use alloy_primitives::B256;

        use crate::arm::{
            custody::HotWalletKey,
            proofs::SubmitSuppressionClear,
            suppression::SuppressionFileStore,
            testkit as tk,
            witness::{AuthorizedCandidate, CheckedCandidate, PairedSubmission},
        };

        let now = 1_000;
        let dir = tk::TempDir::new("simulation-worker-submit");
        let code_hash = B256::repeat_byte(0x33);
        let (validated, victim) = tk::validated_tx(tk::EXECUTOR);
        let candidate = CheckedCandidate::new(validated, campaign());
        let (claim, store_identity) = tk::victim_claim(&dir.path, victim, campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deployment = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            store_identity,
        );
        let suppression_path = tk::write_suppression_file(&dir.path, 5, false);
        let suppression_file = SuppressionFileStore::new(&suppression_path);
        let epoch_store = tk::epoch_store(&dir.path);
        let suppression =
            SubmitSuppressionClear::read(&suppression_file, &epoch_store).expect("suppression");
        let authorized = AuthorizedCandidate::issue_checked(
            true,
            suppression,
            tk::g7(campaign(), now + 100, now),
            claim,
            tk::live_windowed(campaign(), now - 500, now + 100, now),
            deployment,
            candidate,
        )
        .expect("authorized candidate");
        let (key, address) = tk::hot_wallet_key();
        let wallet_path = tk::write_hot_wallet(&dir.path, &key);
        let signed = authorized
            .load_and_sign_with(&tk::sink(&dir.path), || {
                HotWalletKey::load_from(&wallet_path, address)
            })
            .expect("signed submission");

        SubmissionAttempt::Initial(PairedSubmission::assemble(signed))
    }

    fn gated_worker() -> (SimulationWorker, Arc<Barrier>, Arc<Barrier>) {
        let (sender, receiver) = sync_channel::<AdmittedAttempt>(1);
        let release = Arc::new(Barrier::new(2));
        let consumed = Arc::new(Barrier::new(2));
        let worker_release = Arc::clone(&release);
        let worker_consumed = Arc::clone(&consumed);
        let thread = std::thread::spawn(move || {
            worker_release.wait();
            if let Ok(admitted) = receiver.recv() {
                drop(admitted);
                worker_consumed.wait();
            }
            while receiver.recv().is_ok() {}
        });
        (
            SimulationWorker {
                sender: Some(sender),
                admission: Arc::new(AtomicU8::new(ADMISSION_FREE)),
                entrypoint: Arc::new(SimulationEntrypoint::ready()),
                thread: Some(thread),
            },
            release,
            consumed,
        )
    }

    fn idle_worker() -> (SimulationWorker, Arc<AtomicBool>) {
        let (sender, receiver) = sync_channel::<AdmittedAttempt>(1);
        let terminated = Arc::new(AtomicBool::new(false));
        let worker_terminated = Arc::clone(&terminated);
        let thread = std::thread::spawn(move || {
            while receiver.recv().is_ok() {}
            worker_terminated.store(true, Ordering::Release);
        });
        (
            SimulationWorker {
                sender: Some(sender),
                admission: Arc::new(AtomicU8::new(ADMISSION_FREE)),
                entrypoint: Arc::new(SimulationEntrypoint::ready()),
                thread: Some(thread),
            },
            terminated,
        )
    }

    fn forced_panicking_worker() -> (SimulationWorker, Arc<Barrier>, Arc<Barrier>) {
        let (sender, receiver) = sync_channel::<AdmittedAttempt>(1);
        let admission = Arc::new(AtomicU8::new(ADMISSION_FREE));
        let entrypoint = Arc::new(SimulationEntrypoint::ready());
        let release = Arc::new(Barrier::new(2));
        let closed = Arc::new(Barrier::new(2));
        let worker_admission = Arc::clone(&admission);
        let worker_entrypoint = Arc::clone(&entrypoint);
        let worker_release = Arc::clone(&release);
        let worker_closed = Arc::clone(&closed);
        let thread = std::thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                worker_release.wait();
                drop(receiver);
                panic!("forced worker termination");
            }));
            assert!(outcome.is_err());
            worker_entrypoint.close_worker(&worker_admission, true);
            worker_closed.wait();
        });
        (
            SimulationWorker { sender: Some(sender), admission, entrypoint, thread: Some(thread) },
            release,
            closed,
        )
    }

    #[test]
    fn busy_race_admits_exactly_one_preparation() {
        const CONTENDERS: usize = 8;
        let (worker, _) = idle_worker();
        let worker = Arc::new(worker);
        let start = Arc::new(Barrier::new(CONTENDERS + 1));
        let release = Arc::new(Barrier::new(CONTENDERS + 1));
        let mut contenders = Vec::new();
        for _ in 0..CONTENDERS {
            let worker = Arc::clone(&worker);
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            contenders.push(std::thread::spawn(move || {
                start.wait();
                let reservation = worker.try_reserve();
                release.wait();
                reservation
            }));
        }
        start.wait();
        release.wait();
        let admitted = contenders
            .into_iter()
            .map(|thread| thread.join().expect("contender").is_ok())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1);
        drop(worker);
    }

    #[test]
    fn closed_or_full_status_refuses_before_preparation() {
        let (worker, _) = idle_worker();
        *worker.entrypoint.status.lock().expect("status") =
            SimulationEntrypointStatus::LedgerClosed(
                SimulationLedgerClosure::InvalidExistingLedger {
                    epoch: None,
                    class: super::super::SimulationLedgerInvalid::Schema,
                },
            );
        assert!(matches!(worker.try_reserve(), Err(SimulationReservationError::Closed)));
    }

    #[test]
    fn closed_queue_refuses_before_preparation() {
        let (mut worker, _) = idle_worker();
        worker.sender.take();
        assert!(matches!(worker.try_reserve(), Err(SimulationReservationError::Closed)));
    }

    #[test]
    fn unused_reservation_releases_admission() {
        let (worker, _) = idle_worker();
        let reservation = worker.try_reserve().expect("reservation");
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_OCCUPIED);
        drop(reservation);
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_FREE);
    }

    #[test]
    fn submit_rejects_foreign_reservation_and_releases_it() {
        let (worker, _) = idle_worker();
        let foreign_admission = Arc::new(AtomicU8::new(ADMISSION_OCCUPIED));
        let foreign = SimulationReservation { admission: Arc::clone(&foreign_admission) };

        assert_eq!(
            worker.submit(foreign, attempt()),
            Err(SimulationSubmitError::AdmissionInvariant),
        );
        assert_eq!(foreign_admission.load(Ordering::Acquire), ADMISSION_FREE);
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_FREE);
    }

    #[test]
    fn submit_classifies_disconnected_sender_and_releases_reservation() {
        let (sender, receiver) = sync_channel::<AdmittedAttempt>(1);
        drop(receiver);
        let worker = SimulationWorker {
            sender: Some(sender),
            admission: Arc::new(AtomicU8::new(ADMISSION_FREE)),
            entrypoint: Arc::new(SimulationEntrypoint::ready()),
            thread: None,
        };
        let reservation = worker.try_reserve().expect("reservation");

        assert_eq!(worker.submit(reservation, attempt()), Err(SimulationSubmitError::Closed));
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_FREE);
    }

    #[test]
    fn submit_classifies_impossible_full_and_releases_failed_reservation() {
        let (worker, release, consumed) = gated_worker();
        let queued_reservation = worker.try_reserve().expect("reservation");
        worker
            .sender
            .as_ref()
            .expect("sender")
            .try_send(AdmittedAttempt { attempt: attempt(), _reservation: queued_reservation })
            .expect("prime capacity-one queue");
        let impossible_second = SimulationReservation { admission: Arc::clone(&worker.admission) };

        assert_eq!(
            worker.submit(impossible_second, attempt()),
            Err(SimulationSubmitError::AdmissionInvariant),
        );
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_FREE);
        release.wait();
        consumed.wait();
        drop(worker);
    }

    #[test]
    fn successful_submit_holds_admission_until_worker_consumes_transfer() {
        let (worker, release, consumed) = gated_worker();
        let reservation = worker.try_reserve().expect("reservation");

        assert_eq!(worker.submit(reservation, attempt()), Ok(()));
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_OCCUPIED);
        release.wait();
        consumed.wait();
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_FREE);
        drop(worker);
    }

    #[test]
    fn forced_worker_panic_closes_status_and_refuses_preparation() {
        let (worker, release, closed) = forced_panicking_worker();
        release.wait();
        closed.wait();

        assert_eq!(
            worker.status(),
            SimulationEntrypointStatus::Unavailable(
                SimulationEntrypointUnavailable::WorkerClosed { panicked: true },
            ),
        );
        assert_eq!(worker.admission.load(Ordering::Acquire), ADMISSION_CLOSED);
        assert!(matches!(worker.try_reserve(), Err(SimulationReservationError::Closed)));
    }

    #[test]
    fn join_panic_is_preserved_as_bounded_worker_evidence() {
        let (sender, receiver) = sync_channel::<AdmittedAttempt>(1);
        drop(receiver);
        let admission = Arc::new(AtomicU8::new(ADMISSION_FREE));
        let entrypoint = Arc::new(SimulationEntrypoint::ready());
        let observed = Arc::clone(&entrypoint);
        let thread = std::thread::spawn(|| panic!("forced uncaught worker termination"));
        let worker =
            SimulationWorker { sender: Some(sender), admission, entrypoint, thread: Some(thread) };

        drop(worker);

        assert_eq!(
            observed.status(),
            SimulationEntrypointStatus::Unavailable(
                SimulationEntrypointUnavailable::WorkerClosed { panicked: true },
            ),
        );
    }

    #[test]
    fn normal_owner_shutdown_joins_without_worker_failure_status() {
        let (worker, terminated) = idle_worker();
        let observed = Arc::clone(&worker.entrypoint);
        drop(worker);
        assert!(terminated.load(Ordering::Acquire));
        assert_eq!(observed.status(), SimulationEntrypointStatus::Ready);
    }
}
