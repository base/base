//! High-level simulation-only submission entrypoint and typed unavailable sink.

use std::{
    sync::{
        Arc, Mutex,
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
    /// Compile-pinned arm runtime could not open.
    ArmRuntimeUnavailable,
    /// No node-bound settled drawdown authority exists.
    DrawdownAuthorityUnavailable,
    /// No node-bound canonical committed-state authority exists.
    CommittedStateAuthorityUnavailable,
    /// Verified campaign attestations are unavailable.
    CampaignAttestationsUnavailable,
    /// Attested R9 claim store is unavailable.
    ClaimStoreUnavailable,
    /// Process/store deployment identity could not install.
    DeploymentIdentityUnavailable,
    /// Durable local ledger could not open.
    PersistenceUnavailable,
    /// Entrypoint status mutex was poisoned.
    StatusPoisoned,
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
    pub(crate) const fn from_store_open_error(error: super::SimulationStoreOpenError) -> Self {
        match error {
            super::SimulationStoreOpenError::InvalidExistingLedger(class) => {
                Self::LedgerClosed(SimulationLedgerClosure::InvalidExistingLedger {
                    epoch: None,
                    class,
                })
            }
            super::SimulationStoreOpenError::Io(_)
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

/// First production caller of `send_gated`, fixed to `SimBackend`.
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

    /// Processes one already-authorized unified-path attempt on its owning OS worker.
    ///
    /// The caller MUST be the dedicated bounded worker; this function is synchronous by design.
    pub fn process(
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
                Ok(persisted) => SimulationEntrypointTerminal::Persisted(persisted),
                Err(error) => self.close(error),
            },
            SubmitOutcome::NoEgress => SimulationEntrypointTerminal::FreshnessClosed,
            SubmitOutcome::LiveLocksClosed(_)
            | SubmitOutcome::InclusionFailed
            | SubmitOutcome::InclusionSentAttributionFailed(_)
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
}

/// Non-blocking capacity-one handoff into the sole simulation egress owner.
#[derive(Debug)]
pub(crate) struct SimulationWorker {
    sender: Option<SyncSender<SubmissionAttempt>>,
    entrypoint: Arc<SimulationEntrypoint>,
    thread: Option<JoinHandle<()>>,
}

/// Typed bounded queue refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimulationQueueError {
    Busy,
    Closed,
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
        let entrypoint = Arc::new(SimulationEntrypoint::ready());
        let worker_entrypoint = Arc::clone(&entrypoint);
        let thread = std::thread::Builder::new().name("base-mev-arm-egress".to_owned()).spawn(
            move || {
                while let Ok(attempt) = receiver.recv() {
                    let freshness = runtime.freshness(&armed);
                    let terminal = worker_entrypoint.process(attempt, &freshness, &mut store);
                    if matches!(
                        terminal,
                        SimulationEntrypointTerminal::LedgerClosed(_)
                            | SimulationEntrypointTerminal::UnexpectedLiveOutcome
                    ) {
                        break;
                    }
                }
            },
        )?;
        Ok(Self { sender: Some(sender), entrypoint, thread: Some(thread) })
    }

    /// Never blocks, replaces, or queues more than one prepared submission.
    pub(crate) fn try_submit(
        &self,
        attempt: SubmissionAttempt,
    ) -> Result<(), SimulationQueueError> {
        let sender = self.sender.as_ref().ok_or(SimulationQueueError::Closed)?;
        sender.try_send(attempt).map_err(|error| match error {
            TrySendError::Full(_) => SimulationQueueError::Busy,
            TrySendError::Disconnected(_) => SimulationQueueError::Closed,
        })
    }

    pub(crate) fn status(&self) -> SimulationEntrypointStatus {
        self.entrypoint.status()
    }
}

impl Drop for SimulationWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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

    /// Probes only existing production prerequisites and returns a typed rejecting sink.
    ///
    /// S2 has no production committed-state authority implementation, so a successfully
    /// opened arm runtime still closes at that measured gap rather than inventing one.
    pub fn probe_production() -> Self {
        let reason = match super::ArmRuntime::open() {
            Ok(_) => SimulationEntrypointUnavailable::CommittedStateAuthorityUnavailable,
            Err(_) => SimulationEntrypointUnavailable::ArmRuntimeUnavailable,
        };
        Self::new(reason)
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
