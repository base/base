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

use super::{
    FreshnessSources, RuntimeBackend, SimBackend, SimulationCorrelationEnvelopeV1,
    SimulationPersistError, SimulationStore, SubmissionAttempt, SubmitOutcome, send_gated,
};

/// Missing production prerequisite. No variant supplies fallback data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationEntrypointUnavailable {
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
        ledger_epoch: super::SimulationLedgerEpoch,
        /// Rejected sequence.
        next_sequence: u64,
        /// Fixed capacity.
        capacity: u64,
    },
    /// A durable publication operation failed.
    PersistenceFailed {
        /// Epoch being written.
        ledger_epoch: super::SimulationLedgerEpoch,
        /// Sequence with unknown durability.
        next_sequence: u64,
        /// Failed operation.
        operation: super::SimulationStoreOperation,
        /// Stable I/O class.
        io_kind: std::io::ErrorKind,
    },
    /// Existing ledger failed strict startup validation.
    InvalidExistingLedger {
        /// Epoch is unavailable when the durable head itself is invalid.
        ledger_epoch: Option<super::SimulationLedgerEpoch>,
        /// Closed structural class.
        class: super::SimulationLedgerInvalid,
    },
}

impl TryFrom<SimulationPersistError> for SimulationLedgerClosure {
    type Error = ();

    fn try_from(error: SimulationPersistError) -> Result<Self, Self::Error> {
        match error {
            SimulationPersistError::Full { epoch, next_sequence, capacity } => {
                Ok(Self::Full { ledger_epoch: epoch, next_sequence, capacity })
            }
            SimulationPersistError::WriteFailed { epoch, next_sequence, operation, kind } => {
                Ok(Self::PersistenceFailed {
                    ledger_epoch: epoch,
                    next_sequence,
                    operation,
                    io_kind: kind,
                })
            }
            SimulationPersistError::MissingEconomics
            | SimulationPersistError::MissingIdentityEvidence
            | SimulationPersistError::Oversize => Err(()),
        }
    }
}

impl SimulationLedgerClosure {
    fn emit(self) {
        match self {
            Self::Full { ledger_epoch, next_sequence, capacity } => tracing::error!(
                closure_reason = "Full",
                ledger_epoch = ?ledger_epoch.as_bytes(),
                next_sequence,
                capacity,
                "simulation ledger closed"
            ),
            Self::PersistenceFailed { ledger_epoch, next_sequence, operation, io_kind } => {
                tracing::error!(
                    closure_reason = "PersistenceFailed",
                    ledger_epoch = ?ledger_epoch.as_bytes(),
                    next_sequence,
                    operation = ?operation,
                    io_kind = ?io_kind,
                    "simulation ledger closed"
                )
            }
            Self::InvalidExistingLedger { ledger_epoch, class } => tracing::error!(
                closure_reason = "InvalidExistingLedger",
                ledger_epoch = ?ledger_epoch.map(|epoch| *epoch.as_bytes()),
                class = ?class,
                "simulation ledger closed"
            ),
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

/// Terminal classification for one accepted prepared submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationEntrypointTerminal {
    /// Attempt was fsync'd and atomically published.
    Persisted {
        /// Complete bounded join identity for the durable record.
        correlation: SimulationCorrelationEnvelopeV1,
    },
    /// Egress-moment freshness refused the attempt.
    FreshnessClosed,
    /// Ledger closed; status contains the distinct operator reason.
    LedgerClosed(SimulationLedgerClosure),
    /// Simulation-only entrypoint observed an invalid record or live-only outcome.
    UnexpectedLiveOutcome,
}

/// Shared simulation-only execution/status seam installed by the production worker after the
/// complete authority bundle has moved into that worker.
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
            return self.close_persistence(error);
        }
        match self.status() {
            SimulationEntrypointStatus::Ready => {}
            SimulationEntrypointStatus::LedgerClosed(reason) => {
                return SimulationEntrypointTerminal::LedgerClosed(reason);
            }
            SimulationEntrypointStatus::Unavailable(_) => {
                return SimulationEntrypointTerminal::UnexpectedLiveOutcome;
            }
        }
        match send_gated(attempt, freshness, RuntimeBackend::simulated(&self.backend)) {
            SubmitOutcome::Simulated(record) => match store.append(&record) {
                Ok(persisted) => {
                    if let Err(error) = store.ensure_capacity() {
                        self.set_closed(error);
                    }
                    SimulationEntrypointTerminal::Persisted {
                        correlation: *persisted.correlation(),
                    }
                }
                Err(error) => self.close_persistence(error),
            },
            SubmitOutcome::NoEgress => SimulationEntrypointTerminal::FreshnessClosed,
            SubmitOutcome::LiveLocksClosed(_)
            | SubmitOutcome::InclusionFailed(_)
            | SubmitOutcome::InclusionSentAttributionFailed { .. }
            | SubmitOutcome::LiveComplete => SimulationEntrypointTerminal::UnexpectedLiveOutcome,
        }
    }

    fn close_persistence(&self, error: SimulationPersistError) -> SimulationEntrypointTerminal {
        let Ok(reason) = SimulationLedgerClosure::try_from(error) else {
            return SimulationEntrypointTerminal::UnexpectedLiveOutcome;
        };
        SimulationEntrypointTerminal::LedgerClosed(self.set_ledger_closed(reason))
    }

    fn set_closed(&self, error: SimulationPersistError) {
        if let Ok(reason) = SimulationLedgerClosure::try_from(error) {
            self.set_ledger_closed(reason);
        }
    }

    fn set_ledger_closed(&self, proposed: SimulationLedgerClosure) -> SimulationLedgerClosure {
        let Ok(mut status) = self.status.lock() else {
            return proposed;
        };
        match *status {
            SimulationEntrypointStatus::LedgerClosed(reason) => reason,
            SimulationEntrypointStatus::Ready => {
                *status = SimulationEntrypointStatus::LedgerClosed(proposed);
                proposed.emit();
                proposed
            }
            SimulationEntrypointStatus::Unavailable(_) => proposed,
        }
    }

    /// Latches an exact durable ledger closure for process-lifetime status.
    pub fn close_ledger(&self, reason: SimulationLedgerClosure) {
        self.set_ledger_closed(reason);
    }

    fn close_worker(&self, admission: &AtomicU8, panicked: bool) {
        if let Ok(mut status) = self.status.lock()
            && *status == SimulationEntrypointStatus::Ready
        {
            *status = SimulationEntrypointStatus::Unavailable(
                SimulationEntrypointUnavailable::WorkerClosed { panicked },
            );
        }
        admission.store(ADMISSION_CLOSED, Ordering::Release);
    }
}

/// Test-only legacy attempt worker retained for focused entrypoint persistence tests.
#[cfg(test)]
#[derive(Debug)]
pub struct SimulationWorker {
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
pub struct SimulationReservation {
    admission: Arc<AtomicU8>,
}

/// Typed pre-preparation admission refusal.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationReservationError {
    /// The sole running-or-queued admission slot is occupied.
    Busy,
    /// The worker or ledger is closed.
    Closed,
}

/// A post-preparation submission can fail only because the worker closed or its invariant broke;
/// queue fullness is deliberately not an exposed outcome.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationSubmitError {
    /// The worker closed before accepting the reserved attempt.
    Closed,
    /// The reservation did not belong to this worker or the bounded queue invariant failed.
    AdmissionInvariant,
}

#[cfg(test)]
#[derive(Debug)]
struct AdmittedAttempt {
    attempt: SubmissionAttempt,
    _reservation: SimulationReservation,
}

const ADMISSION_FREE: u8 = 0;
const ADMISSION_OCCUPIED: u8 = 1;
const ADMISSION_CLOSED: u8 = 2;

impl SimulationReservation {
    /// Creates the reservation moved with the unified production handoff payload.
    pub(crate) fn from_admission(admission: Arc<AtomicU8>) -> Self {
        Self { admission }
    }
}
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

#[cfg(test)]
impl SimulationWorker {
    /// Spawns the only thread permitted to call the synchronous simulation entrypoint.
    pub fn spawn<C, D>(
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
    pub fn try_reserve(&self) -> Result<SimulationReservation, SimulationReservationError> {
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
    pub fn submit(
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

    /// Returns the sticky operator-visible worker status.
    pub fn status(&self) -> SimulationEntrypointStatus {
        self.entrypoint.status()
    }
}

#[cfg(test)]
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
    fn epoch(byte: u8) -> super::super::SimulationLedgerEpoch {
        super::super::SimulationLedgerEpoch::for_test([byte; 32])
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
        let authorized = AuthorizedCandidate::issue_with_gate_for_test(
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
    fn closed_status_refuses_before_preparation() {
        let (worker, _) = idle_worker();
        worker.entrypoint.set_ledger_closed(SimulationLedgerClosure::InvalidExistingLedger {
            ledger_epoch: None,
            class: super::super::SimulationLedgerInvalid::Schema,
        });
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

    #[test]
    fn full_reason_remains_queryable() {
        let entrypoint = SimulationEntrypoint::ready();
        let reason = SimulationLedgerClosure::Full {
            ledger_epoch: epoch(1),
            next_sequence: 262_144,
            capacity: 262_144,
        };

        assert_eq!(entrypoint.set_ledger_closed(reason), reason);
        assert_eq!(entrypoint.status(), SimulationEntrypointStatus::LedgerClosed(reason));
    }

    #[test]
    fn persistence_failed_reason_remains_queryable() {
        let entrypoint = SimulationEntrypoint::ready();
        let reason = SimulationLedgerClosure::PersistenceFailed {
            ledger_epoch: epoch(2),
            next_sequence: 7,
            operation: super::super::SimulationStoreOperation::UpdateHead,
            io_kind: std::io::ErrorKind::WriteZero,
        };

        assert_eq!(entrypoint.set_ledger_closed(reason), reason);
        assert_eq!(entrypoint.status(), SimulationEntrypointStatus::LedgerClosed(reason));
    }

    #[test]
    fn invalid_existing_ledger_reason_remains_queryable() {
        let entrypoint = SimulationEntrypoint::ready();
        let reason = SimulationLedgerClosure::InvalidExistingLedger {
            ledger_epoch: Some(epoch(3)),
            class: super::super::SimulationLedgerInvalid::HashChain,
        };

        assert_eq!(entrypoint.set_ledger_closed(reason), reason);
        assert_eq!(entrypoint.status(), SimulationEntrypointStatus::LedgerClosed(reason));
    }

    #[test]
    fn first_ledger_closure_reason_wins() {
        let entrypoint = SimulationEntrypoint::ready();
        let first = SimulationLedgerClosure::Full {
            ledger_epoch: epoch(4),
            next_sequence: 262_144,
            capacity: 262_144,
        };
        let later = SimulationLedgerClosure::PersistenceFailed {
            ledger_epoch: epoch(4),
            next_sequence: 262_144,
            operation: super::super::SimulationStoreOperation::SyncDirectory,
            io_kind: std::io::ErrorKind::Other,
        };

        assert_eq!(entrypoint.set_ledger_closed(first), first);
        assert_eq!(entrypoint.set_ledger_closed(later), first);
        assert_eq!(entrypoint.status(), SimulationEntrypointStatus::LedgerClosed(first));
    }

    #[test]
    fn worker_close_and_later_proposal_preserve_first_ledger_closure() {
        let entrypoint = SimulationEntrypoint::ready();
        let admission = AtomicU8::new(ADMISSION_FREE);
        let first = SimulationLedgerClosure::InvalidExistingLedger {
            ledger_epoch: None,
            class: super::super::SimulationLedgerInvalid::Schema,
        };
        let later = SimulationLedgerClosure::PersistenceFailed {
            ledger_epoch: epoch(4),
            next_sequence: 262_144,
            operation: super::super::SimulationStoreOperation::SyncDirectory,
            io_kind: std::io::ErrorKind::Other,
        };
        assert_eq!(entrypoint.set_ledger_closed(first), first);

        entrypoint.close_worker(&admission, true);

        assert_eq!(entrypoint.set_ledger_closed(later), first);
        assert_eq!(entrypoint.status(), SimulationEntrypointStatus::LedgerClosed(first));
        assert_eq!(admission.load(Ordering::Acquire), ADMISSION_CLOSED);
    }

    #[test]
    fn sink_is_closed_while_detailed_status_remains_queryable() {
        let (worker, _) = idle_worker();
        let reason = SimulationLedgerClosure::Full {
            ledger_epoch: epoch(5),
            next_sequence: 262_144,
            capacity: 262_144,
        };
        worker.entrypoint.set_ledger_closed(reason);

        assert!(matches!(worker.try_reserve(), Err(SimulationReservationError::Closed)));
        assert_eq!(worker.status(), SimulationEntrypointStatus::LedgerClosed(reason));
    }
}
