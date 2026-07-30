//! Unified production T4e handoff, bounded admission, startup publication, and worker ownership.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::JoinHandle,
};

use crate::{SealedUnsignedCandidate, T4eCandidateHandoff, T4eHandoffError, TxAuthorityError};

use super::{
    ProductionCandidateError, ProductionClaimFailure, ProductionLatchOutcome,
    ProductionSigningError, SimulationEntrypoint, SimulationEntrypointStatus,
    SimulationEntrypointUnavailable, SimulationLedgerClosure,
};

const ADMISSION_FREE: u8 = 0;
const ADMISSION_OCCUPIED: u8 = 1;
const ADMISSION_CLOSED: u8 = 2;

/// Stable node-local provider installation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionProviderFailure {
    /// Provider access failed.
    Unavailable,
    /// Provider value was invalid or absent.
    Invalid,
    /// Provider value exceeded a fixed bound.
    TooLarge,
}

/// Stable arm-runtime open failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionArmRuntimeOpenFailure {
    /// Kill-state startup validation failed.
    Startup,
    /// Suppression state failed to open.
    Suppression,
}

/// Stable owner-signed campaign bundle failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionCampaignBundleFailure {
    /// The pinned bundle was absent.
    Missing,
    /// A path component or object was unsafe.
    UnsafeObject,
    /// A fixed resource bound was exceeded.
    Bounds,
    /// Canonical decoding failed.
    Decode,
    /// Signature validation failed.
    Signature,
    /// An authenticated identity differed.
    Identity,
    /// Inner attestations came from different generations.
    MixedGeneration,
    /// Filesystem inspection or reading failed.
    Io,
}

/// Stable deployment installation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionDeploymentFailure {
    /// Committed-state access failed.
    ProviderUnavailable,
    /// The committed executor account was absent.
    AccountAbsent,
    /// Committed runtime code was absent.
    CodeMissing,
    /// Runtime code hash differed.
    CodeHashMismatch,
    /// Running binary digest differed.
    BinaryMismatch,
    /// Deployment digest differed.
    DeploymentDigestMismatch,
    /// R9 store identity differed.
    R9IdentityMismatch,
    /// Owner signature failed.
    Signature,
    /// Chain identity differed.
    ChainMismatch,
}

/// Stable arm installation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionArmFailure {
    /// A durable kill reason was active.
    KillReason(base_mev_trader::KillReason),
    /// Fail-stop persistence failed.
    LatchPersistFailed,
    /// Process poison was already active.
    Poisoned,
    /// Authority freshness was unavailable.
    Freshness,
    /// The victim was already claimed.
    AlreadyClaimed,
}

/// Stable simulation-store open failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionStoreOpenFailure {
    /// Filesystem access failed.
    Io,
    /// Another process already owns the store.
    AlreadyOpen,
    /// Lease validation failed.
    Lease,
    /// Existing ledger structure was invalid.
    InvalidExistingLedger {
        /// Whether an authenticated epoch was recoverable.
        trusted_epoch_present: bool,
        /// Exact structural class.
        class: super::SimulationLedgerInvalid,
    },
}

/// Stable shared-bridge installation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductionBridgeFailure {
    /// Candidate assembly authority failed.
    Assembly(TxAuthorityError),
    /// Structural bindings were rejected.
    BindingRejected,
    /// Candidate came from another installation.
    CrossInstallation,
    /// Captured snapshot was stale.
    SnapshotStale,
    /// Execution freshness was unavailable.
    FreshnessUnavailable,
    /// Execution identity changed.
    IdentityChanged,
    /// Candidate lifecycle was cancelled.
    Cancelled,
    /// Candidate deadline elapsed.
    Deadline,
}

/// Stable production installation failure exposed without paths or source errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductionSimulationInstallError {
    /// The startup acknowledgement has not completed.
    InstallationInProgress,
    /// Activation inputs violated the exact conjunction.
    ActivationInvariant,
    /// Arm runtime could not open.
    ArmRuntimeUnavailable(ProductionArmRuntimeOpenFailure),
    /// Committed-state authority was unavailable.
    CommittedStateUnavailable(ProductionProviderFailure),
    /// Settled-loss authority was unavailable.
    DrawdownAuthorityUnavailable(super::SettledLossUnavailableReason),
    /// Campaign proof bundle was unavailable.
    CampaignBundleUnavailable(ProductionCampaignBundleFailure),
    /// Claim store could not open with the attested identity.
    ClaimStoreUnavailable(ProductionClaimFailure),
    /// Deployment identity could not be established.
    DeploymentIdentityUnavailable(ProductionDeploymentFailure),
    /// Custody preflight failed.
    CustodyUnavailable(super::ProductionCustodyFailure),
    /// Fail sink could not open clear.
    FailSinkUnavailable(ProductionArmFailure),
    /// Production arming criteria were unavailable.
    ArmingUnavailable(ProductionArmFailure),
    /// Simulation persistence could not open.
    PersistenceUnavailable(ProductionStoreOpenFailure),
    /// Durable ledger capacity was unavailable before publication.
    CapacityUnavailable(SimulationLedgerClosure),
    /// Shared bridge installation failed.
    BridgeUnavailable(ProductionBridgeFailure),
    /// The worker OS thread could not be created.
    WorkerSpawnUnavailable,
    /// The startup acknowledgement failed.
    WorkerStartupUnavailable(WorkerStartupFailure),
}

/// Stable failure of the one-shot startup acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStartupFailure {
    /// The five-second startup deadline elapsed.
    Timeout,
    /// The sole startup sender disconnected.
    SenderLost,
}

/// Stable worker persistence failure that is not a ledger closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionPersistenceFailure {
    /// The record did not retain by-value economics.
    MissingEconomics,
    /// The record did not retain bounded identity evidence.
    MissingIdentityEvidence,
    /// The record exceeded its fixed bound.
    Oversize,
}

/// Stable worker failure after a candidate has been admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductionWorkerError {
    /// The victim was already claimed; no latch was attempted.
    ClaimAlreadyClaimed,
    /// The claim store failed and the mandatory latch was attempted.
    ClaimError {
        /// Stable source class.
        source: ProductionClaimFailure,
        /// Mandatory latch result.
        latch: ProductionLatchOutcome,
    },
    /// Custody or signing failed.
    Signing(ProductionSigningError),
    /// Paired-submission assembly failed.
    Assembly(TxAuthorityError),
    /// A bounded record was invalid before persistence.
    Persistence(ProductionPersistenceFailure),
    /// A simulation-only worker observed a live-only transport result.
    UnexpectedLiveOutcome,
    /// Candidate processing unwound.
    WorkerPanic,
}

/// Sticky reason the unified handoff stopped accepting candidates after publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductionHandoffClosed {
    /// The worker receiver disconnected.
    Disconnected,
    /// Capacity-one reservation and queue state diverged.
    AdmissionInvariant,
    /// The durable simulation ledger closed.
    LedgerClosed,
    /// Checked proof or authorization failed.
    Authorization(ProductionCandidateError),
    /// A later worker phase failed.
    Worker(ProductionWorkerError),
}

/// Sole public live status of production T4e simulation installation and processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductionSimulationHandoffStatus {
    /// Startup has not acknowledged readiness.
    Installing,
    /// Installation failed before publication.
    Unavailable(Arc<ProductionSimulationInstallError>),
    /// The worker is open and the entrypoint remains ready.
    Ready,
    /// The entrypoint became unavailable after publication.
    EntrypointUnavailable(SimulationEntrypointUnavailable),
    /// The durable ledger closed after publication.
    LedgerClosed(SimulationLedgerClosure),
    /// The handoff closed for an exact worker/admission reason.
    HandoffClosed(ProductionHandoffClosed),
}

/// Capacity-one payload transferring candidate and reservation together by value.
#[derive(Debug)]
pub struct AdmittedCandidate {
    candidate: SealedUnsignedCandidate,
    reservation: ProductionReservation,
}

impl AdmittedCandidate {
    /// Destructures the sole queue payload for ordered worker processing.
    pub fn into_parts(self) -> (SealedUnsignedCandidate, ProductionReservation) {
        (self.candidate, self.reservation)
    }
}

/// Linear ownership of the sole running-or-queued production admission slot.
#[derive(Debug)]
pub struct ProductionReservation {
    admission: Arc<AtomicU8>,
}

impl Drop for ProductionReservation {
    fn drop(&mut self) {
        let _ = self.admission.compare_exchange(
            ADMISSION_OCCUPIED,
            ADMISSION_FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[derive(Debug)]
pub enum ProductionHandoffState {
    /// Startup has not acknowledged readiness.
    Installing(Arc<ProductionSimulationInstallError>),
    /// Installation failed before readiness.
    Unavailable(Arc<ProductionSimulationInstallError>),
    /// The sole sender, entrypoint, and admission slot are published together.
    Open {
        /// Shared simulation entrypoint.
        entrypoint: Arc<SimulationEntrypoint>,
        /// Capacity-one admission state.
        admission: Arc<AtomicU8>,
        /// Sole candidate sender.
        sender: SyncSender<AdmittedCandidate>,
    },
    /// Acceptance is permanently closed.
    Closed {
        /// Published entrypoint, when startup reached Open.
        entrypoint: Option<Arc<SimulationEntrypoint>>,
        /// Exact sticky closure reason.
        reason: ProductionHandoffClosed,
    },
}

/// Shared state behind the sole production handoff allocation.
#[derive(Debug)]
pub struct ProductionHandoffShared {
    state: Mutex<ProductionHandoffState>,
}

/// The exact-one production implementation of [`T4eCandidateHandoff`].
#[derive(Debug)]
pub struct ProductionSimulationHandoff {
    shared: Arc<ProductionHandoffShared>,
}

impl ProductionSimulationHandoff {
    /// Installs the exact initial shared state without publishing readiness.
    pub fn install(state: ProductionHandoffState) -> Self {
        Self { shared: Arc::new(ProductionHandoffShared { state: Mutex::new(state) }) }
    }

    /// Creates the one rejecting handoff used for a typed pre-spawn installation failure.
    pub fn unavailable(error: ProductionSimulationInstallError) -> Arc<Self> {
        Arc::new(Self::install(ProductionHandoffState::Unavailable(Arc::new(error))))
    }

    /// Returns the sole live production status with sticky entrypoint precedence.
    pub fn status(&self) -> ProductionSimulationHandoffStatus {
        let Ok(state) = self.shared.state.lock() else {
            return ProductionSimulationHandoffStatus::EntrypointUnavailable(
                SimulationEntrypointUnavailable::StatusPoisoned,
            );
        };
        match &*state {
            ProductionHandoffState::Installing(_) => ProductionSimulationHandoffStatus::Installing,
            ProductionHandoffState::Unavailable(error) => {
                ProductionSimulationHandoffStatus::Unavailable(Arc::clone(error))
            }
            ProductionHandoffState::Open { entrypoint, .. } => match entrypoint.status() {
                SimulationEntrypointStatus::Ready => ProductionSimulationHandoffStatus::Ready,
                SimulationEntrypointStatus::Unavailable(reason) => {
                    ProductionSimulationHandoffStatus::EntrypointUnavailable(reason)
                }
                SimulationEntrypointStatus::LedgerClosed(reason) => {
                    ProductionSimulationHandoffStatus::LedgerClosed(reason)
                }
            },
            ProductionHandoffState::Closed { entrypoint, reason } => {
                if let Some(entrypoint) = entrypoint {
                    match entrypoint.status() {
                        SimulationEntrypointStatus::LedgerClosed(reason) => {
                            return ProductionSimulationHandoffStatus::LedgerClosed(reason);
                        }
                        SimulationEntrypointStatus::Unavailable(reason) => {
                            return ProductionSimulationHandoffStatus::EntrypointUnavailable(
                                reason,
                            );
                        }
                        SimulationEntrypointStatus::Ready => {}
                    }
                }
                ProductionSimulationHandoffStatus::HandoffClosed(reason.clone())
            }
        }
    }

    /// Monotonically closes acceptance and drops the sole candidate sender.
    pub fn close(&self, reason: ProductionHandoffClosed) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        match &*state {
            ProductionHandoffState::Closed { .. } => return,
            ProductionHandoffState::Installing(_) | ProductionHandoffState::Unavailable(_) => {
                *state = ProductionHandoffState::Closed { entrypoint: None, reason };
            }
            ProductionHandoffState::Open { entrypoint, admission, .. } => {
                admission.store(ADMISSION_CLOSED, Ordering::Release);
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason,
                };
            }
        }
    }
}

impl T4eCandidateHandoff for ProductionSimulationHandoff {
    fn try_handoff(&self, candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {
        let Ok(mut state) = self.shared.state.lock() else {
            return Err(T4eHandoffError::Closed);
        };
        let ProductionHandoffState::Open { admission, sender, entrypoint } = &*state else {
            return match &*state {
                ProductionHandoffState::Installing(_) | ProductionHandoffState::Unavailable(_) => {
                    Err(T4eHandoffError::Rejected)
                }
                ProductionHandoffState::Closed { .. } => Err(T4eHandoffError::Closed),
                ProductionHandoffState::Open { .. } => unreachable!("matched open state"),
            };
        };
        if entrypoint.status() != SimulationEntrypointStatus::Ready {
            return Err(T4eHandoffError::Closed);
        }
        match admission.compare_exchange(
            ADMISSION_FREE,
            ADMISSION_OCCUPIED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(ADMISSION_OCCUPIED) => return Err(T4eHandoffError::Busy),
            Err(ADMISSION_CLOSED) => return Err(T4eHandoffError::Closed),
            Err(invalid) => {
                debug_assert_ne!(invalid, ADMISSION_FREE);
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason: ProductionHandoffClosed::AdmissionInvariant,
                };
                return Err(T4eHandoffError::Closed);
            }
        }
        let reservation = ProductionReservation { admission: Arc::clone(admission) };
        match sender.try_send(AdmittedCandidate { candidate, reservation }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason: ProductionHandoffClosed::Disconnected,
                };
                Err(T4eHandoffError::Closed)
            }
            Err(TrySendError::Full(_)) => {
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason: ProductionHandoffClosed::AdmissionInvariant,
                };
                Err(T4eHandoffError::Closed)
            }
        }
    }
}

/// Private-publication capability moved exactly once into the production worker closure.
#[derive(Debug)]
pub struct ProductionHandoffInstaller {
    handoff: Arc<ProductionSimulationHandoff>,
    admission: Arc<AtomicU8>,
    sender: SyncSender<AdmittedCandidate>,
    receiver: Receiver<AdmittedCandidate>,
}

/// Sole candidate receiver retained only by the production worker.
#[derive(Debug)]
pub struct ProductionCandidateReceiver {
    receiver: Receiver<AdmittedCandidate>,
}

impl ProductionCandidateReceiver {
    /// Receives the next admitted candidate in strict channel order.
    pub fn receive(&self) -> Result<AdmittedCandidate, ()> {
        self.receiver.recv().map_err(|_| ())
    }
}

impl ProductionHandoffInstaller {
    /// Creates Installing state and one capacity-one candidate channel.
    pub fn new() -> Self {
        let (sender, receiver) = sync_channel(1);
        let admission = Arc::new(AtomicU8::new(ADMISSION_FREE));
        let handoff = Arc::new(ProductionSimulationHandoff {
            shared: Arc::new(ProductionHandoffShared {
                state: Mutex::new(ProductionHandoffState::Installing(Arc::new(
                    ProductionSimulationInstallError::InstallationInProgress,
                ))),
            }),
        });
        Self { handoff, admission, sender, receiver }
    }

    /// Returns the sole handoff allocation before the installer moves into the worker.
    pub fn handoff(&self) -> Arc<ProductionSimulationHandoff> {
        Arc::clone(&self.handoff)
    }

    /// Consumes the sole publication capability and opens the worker queue exactly once.
    pub fn publish_ready(
        self,
        entrypoint: Arc<SimulationEntrypoint>,
    ) -> Result<ProductionCandidateReceiver, ()> {
        let Self { handoff, admission, sender, receiver } = self;
        {
            let mut state = handoff.shared.state.lock().map_err(|_| ())?;
            match &*state {
                ProductionHandoffState::Installing(_) => {
                    *state = ProductionHandoffState::Open { entrypoint, admission, sender };
                }
                ProductionHandoffState::Unavailable(_)
                | ProductionHandoffState::Open { .. }
                | ProductionHandoffState::Closed { .. } => return Err(()),
            }
        }
        Ok(ProductionCandidateReceiver { receiver })
    }
}

impl Default for ProductionHandoffInstaller {
    fn default() -> Self {
        Self::new()
    }
}

/// Non-clone owner of the sole production worker OS thread.
#[derive(Debug)]
pub struct ProductionSimulationWorkerOwner {
    thread: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    handoff: Arc<ProductionSimulationHandoff>,
}

impl ProductionSimulationWorkerOwner {
    /// Constructs the sole owner after successful OS-thread spawn.
    pub fn new(
        thread: JoinHandle<()>,
        cancel: Arc<AtomicBool>,
        handoff: Arc<ProductionSimulationHandoff>,
    ) -> Self {
        Self { thread, cancel, handoff }
    }

    /// Closes acceptance, requests cancellation, and returns the sole join handle.
    pub fn into_join_handle(self) -> JoinHandle<()> {
        self.cancel.store(true, Ordering::Release);
        self.handoff.close(ProductionHandoffClosed::Disconnected);
        self.thread
    }
}
