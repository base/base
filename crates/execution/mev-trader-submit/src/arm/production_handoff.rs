//! Unified production T4e handoff, bounded admission, startup publication, and worker ownership.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::JoinHandle,
    time::Duration,
};

use base_mev_trader::{ArmedCriteria, ClaimStoreError, VictimClaimConfig, VictimClaimStore};

use crate::{
    BridgeError, InstalledSubmissionBridge, SealedUnsignedCandidate, T4eCandidateHandoff,
    T4eHandoffError, TxAuthorityError,
};

use super::{
    CHAIN_ID_BASE, NodeLocalSettledLossAuthority, PreparedSettledLossAuthority,
    ProductionB5Runtime, ProductionB5RuntimeInstallError, ProductionCandidateError,
    ProductionClaimFailure, ProductionClaimResult, ProductionCodeHashProvider,
    ProductionLatchOutcome, ProductionProofBundle, ProductionSigningError, R9_CLAIM_STORE_PATH,
    RuntimeBackend, SIMULATION_RECORD_CAPACITY, SimBackend, SimulationEntrypoint,
    SimulationEntrypointStatus, SimulationEntrypointUnavailable, SimulationLedgerClosure,
    SimulationPersistError, SimulationReservation, SimulationStore, SimulationWorker,
    SubmitOutcome, production_custody_preflight, send_gated, try_claim_detailed,
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

/// One-shot worker startup acknowledgement.
#[derive(Debug)]
pub enum WorkerStartup {
    /// The complete bundle is owned by the worker and its entrypoint is ready.
    Ready {
        /// Shared entrypoint created inside the worker.
        entrypoint: Arc<SimulationEntrypoint>,
        /// Sole candidate sender moved to node-start for publication.
        sender: SyncSender<AdmittedCandidate>,
        /// Shared capacity-one admission state.
        admission: Arc<AtomicU8>,
    },
    /// Worker initialization failed before readiness.
    Failed(Arc<ProductionSimulationInstallError>),
}

/// Private non-clone sender whose send consumes the only producer.
#[derive(Debug)]
struct WorkerStartupSender(SyncSender<WorkerStartup>);

impl WorkerStartupSender {
    fn send(self, startup: WorkerStartup) -> Result<(), WorkerStartup> {
        self.0.send(startup).map_err(|error| error.0)
    }
}

/// Worker-owned pre-publication queue and one-shot acknowledgement.
#[derive(Debug)]
pub struct ProductionWorkerBootstrap {
    admission: Arc<AtomicU8>,
    sender: SyncSender<AdmittedCandidate>,
    receiver: Receiver<AdmittedCandidate>,
    startup: WorkerStartupSender,
}

impl ProductionWorkerBootstrap {
    /// Acknowledges readiness exactly once and retains the sole candidate receiver.
    pub fn publish_ready(
        self,
        entrypoint: Arc<SimulationEntrypoint>,
    ) -> Result<ProductionCandidateReceiver, WorkerStartup> {
        let Self { admission, sender, receiver, startup } = self;
        startup.send(WorkerStartup::Ready { entrypoint, sender, admission })?;
        Ok(ProductionCandidateReceiver { receiver })
    }

    /// Sends a typed startup failure exactly once.
    pub fn publish_failed(
        self,
        error: Arc<ProductionSimulationInstallError>,
    ) -> Result<(), WorkerStartup> {
        self.startup.send(WorkerStartup::Failed(error))
    }
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
    /// Shared bridge conversion failed.
    Bridge(ProductionBridgeFailure),
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

impl ProductionHandoffClosed {
    /// Returns whether the reason consumes only the current candidate while the worker stays ready.
    pub const fn is_candidate_denial(&self) -> bool {
        matches!(
            self,
            Self::Authorization(_)
                | Self::Worker(
                    ProductionWorkerError::Bridge(_) | ProductionWorkerError::ClaimAlreadyClaimed
                )
        )
    }
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

/// Node-local handles required to load the complete production bundle.
#[derive(Debug)]
pub struct ProductionBundleInputs<C, F> {
    /// Installed committed-state authority.
    pub committed_state: Arc<C>,
    /// Installed finalized-chain authority.
    pub finalized_chain: Arc<F>,
    /// The one shared T4d/T4e bridge allocation.
    pub bridge: Arc<InstalledSubmissionBridge>,
    /// Simulation-only backend.
    pub simulation_backend: SimBackend,
}

/// Complete non-clone bundle moved exactly once into the production worker.
#[derive(Debug)]
pub struct ProductionInstallBundle<C, F>
where
    F: super::FinalizedChainAuthority,
{
    committed_state: Arc<C>,
    bridge: Arc<InstalledSubmissionBridge>,
    simulation_backend: SimBackend,
    proofs: ProductionProofBundle,
    claims: VictimClaimStore,
    prepared_loss: PreparedSettledLossAuthority<Arc<F>>,
    store: SimulationStore,
}

/// Complete bundle plus owner-verified arming criteria moved into thread spawn.
#[derive(Debug)]
pub struct ProductionInstallInputs<C, F>
where
    F: super::FinalizedChainAuthority,
{
    /// Fully loaded production conjunction.
    pub bundle: ProductionInstallBundle<C, F>,
    /// Owner-verified arming criteria.
    pub armed: ArmedCriteria,
}

impl<C, F> ProductionInstallBundle<C, F>
where
    C: super::CommittedStateAuthority,
    F: super::FinalizedChainAuthority,
{
    /// Loads every bounded prerequisite before any worker thread exists.
    pub fn load(
        inputs: ProductionBundleInputs<C, F>,
    ) -> Result<Self, ProductionSimulationInstallError> {
        let ProductionBundleInputs { committed_state, finalized_chain, bridge, simulation_backend } =
            inputs;
        let proofs = ProductionProofBundle::load()
            .map_err(ProductionSimulationInstallError::CampaignBundleUnavailable)?;
        let provider = ProductionCodeHashProvider::install(Arc::clone(&committed_state));
        let deployment = proofs.verify_deployment(&provider).map_err(|error| match error {
            ProductionCandidateError::Deployment(_) => {
                ProductionSimulationInstallError::DeploymentIdentityUnavailable(
                    ProductionDeploymentFailure::ProviderUnavailable,
                )
            }
            ProductionCandidateError::G7(_)
            | ProductionCandidateError::Live(_)
            | ProductionCandidateError::CampaignMismatch
            | ProductionCandidateError::SuppressionUnavailable
            | ProductionCandidateError::Suppressed
            | ProductionCandidateError::SuppressionRollback
            | ProductionCandidateError::SuppressionInvalid
            | ProductionCandidateError::KillActive
            | ProductionCandidateError::SettledLoss(_)
            | ProductionCandidateError::CommittedStateUnavailable
            | ProductionCandidateError::CommittedAccountAbsent
            | ProductionCandidateError::Gate(_) => {
                ProductionSimulationInstallError::ActivationInvariant
            }
        })?;
        let prepared_loss =
            NodeLocalSettledLossAuthority::prepare_complete(Arc::clone(&finalized_chain))
                .map_err(ProductionSimulationInstallError::DrawdownAuthorityUnavailable)?;
        if prepared_loss.campaign_id().as_slice() != proofs.campaign_id().as_bytes() {
            return Err(ProductionSimulationInstallError::ActivationInvariant);
        }
        let claims = VictimClaimStore::open_existing(
            &VictimClaimConfig { db_path: PathBuf::from(R9_CLAIM_STORE_PATH) },
            deployment.r9_store_identity(),
        )
        .map_err(|error| {
            ProductionSimulationInstallError::ClaimStoreUnavailable(map_claim_failure(error))
        })?;
        production_custody_preflight()
            .map_err(ProductionSimulationInstallError::CustodyUnavailable)?;
        let store = SimulationStore::open().map_err(|error| {
            ProductionSimulationInstallError::PersistenceUnavailable(match error {
                super::SimulationStoreOpenError::Io(_) => ProductionStoreOpenFailure::Io,
                super::SimulationStoreOpenError::AlreadyOpen => {
                    ProductionStoreOpenFailure::AlreadyOpen
                }
                super::SimulationStoreOpenError::Lease(_) => ProductionStoreOpenFailure::Lease,
                super::SimulationStoreOpenError::InvalidExistingLedger { ledger_epoch, class } => {
                    ProductionStoreOpenFailure::InvalidExistingLedger {
                        trusted_epoch_present: ledger_epoch.is_some(),
                        class,
                    }
                }
            })
        })?;
        store.ensure_capacity().map_err(|error| {
            let reason = SimulationLedgerClosure::try_from(error)
                .expect("capacity failure is always a ledger closure");
            ProductionSimulationInstallError::CapacityUnavailable(reason)
        })?;
        Ok(Self {
            committed_state,
            bridge,
            simulation_backend,
            proofs,
            claims,
            prepared_loss,
            store,
        })
    }
}

/// Capacity-one payload transferring candidate and reservation together by value.
#[derive(Debug)]
pub struct AdmittedCandidate {
    candidate: SealedUnsignedCandidate,
    reservation: SimulationReservation,
}

impl AdmittedCandidate {
    /// Destructures the sole queue payload for ordered worker processing.
    pub fn into_parts(self) -> (SealedUnsignedCandidate, SimulationReservation) {
        (self.candidate, self.reservation)
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
    denied_candidates: AtomicU64,
}

/// The exact-one production implementation of [`T4eCandidateHandoff`].
#[derive(Debug)]
pub struct ProductionSimulationHandoff {
    shared: Arc<ProductionHandoffShared>,
}

impl ProductionSimulationHandoff {
    /// Installs the exact initial shared state without publishing readiness.
    pub fn install(state: ProductionHandoffState) -> Self {
        Self {
            shared: Arc::new(ProductionHandoffShared {
                state: Mutex::new(state),
                denied_candidates: AtomicU64::new(0),
            }),
        }
    }

    /// Creates the one rejecting handoff used for a typed pre-spawn installation failure.
    pub fn unavailable(error: ProductionSimulationInstallError) -> Arc<Self> {
        Arc::new(Self::install(ProductionHandoffState::Unavailable(Arc::new(error))))
    }

    /// Returns the number of per-candidate denials consumed without closing the worker.
    pub fn candidate_denial_count(&self) -> u64 {
        self.shared.denied_candidates.load(Ordering::Acquire)
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
                return;
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
        let reservation = SimulationReservation::from_admission(Arc::clone(admission));
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
        let handoff =
            Arc::new(ProductionSimulationHandoff::install(ProductionHandoffState::Installing(
                Arc::new(ProductionSimulationInstallError::InstallationInProgress),
            )));
        Self { handoff, admission, sender, receiver }
    }

    /// Returns the sole handoff allocation before the installer moves into the worker.
    pub fn handoff(&self) -> Arc<ProductionSimulationHandoff> {
        Arc::clone(&self.handoff)
    }

    fn into_worker(self, startup: WorkerStartupSender) -> ProductionWorkerBootstrap {
        let Self { handoff: _, admission, sender, receiver } = self;
        ProductionWorkerBootstrap { admission, sender, receiver, startup }
    }

    /// Test-only direct publication seam for real candidate state-machine tests.
    #[cfg(test)]
    pub fn publish_ready_for_test(
        self,
        entrypoint: Arc<SimulationEntrypoint>,
    ) -> Result<ProductionCandidateReceiver, ()> {
        let (startup_sender, startup_receiver) = sync_channel(1);
        let handoff = self.handoff();
        let worker = self.into_worker(WorkerStartupSender(startup_sender));
        let receiver = worker.publish_ready(entrypoint).map_err(|_| ())?;
        let WorkerStartup::Ready { entrypoint, sender, admission } =
            startup_receiver.recv().map_err(|_| ())?
        else {
            return Err(());
        };
        let mut state = handoff.shared.state.lock().map_err(|_| ())?;
        *state = ProductionHandoffState::Open { entrypoint, sender, admission };
        drop(state);
        Ok(receiver)
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

/// Successful worker spawn awaiting its one-shot readiness acknowledgement.
#[derive(Debug)]
pub struct ProductionStartup {
    /// Shared handoff created before thread spawn.
    pub handoff: Arc<ProductionSimulationHandoff>,
    /// Sole worker owner retained through readiness.
    pub owner: ProductionSimulationWorkerOwner,
    /// Sole startup receiver.
    pub receiver: Receiver<WorkerStartup>,
    /// Shared timeout cancellation flag.
    pub cancel: Arc<AtomicBool>,
}

/// Result of attempting to create the production worker OS thread.
#[derive(Debug)]
pub enum ProductionSpawnDisposition {
    /// The worker exists and awaits readiness.
    Spawned(ProductionStartup),
    /// Thread creation failed before an owner existed.
    Unavailable {
        /// Rejecting unified handoff.
        handoff: Arc<ProductionSimulationHandoff>,
        /// Exact stable spawn failure.
        error: Arc<ProductionSimulationInstallError>,
    },
}

/// Result of the bounded startup handshake.
#[derive(Debug)]
pub enum ProductionInstallDisposition {
    /// The complete worker conjunction was published.
    Ready {
        /// Open unified handoff.
        handoff: Arc<ProductionSimulationHandoff>,
        /// Sole worker owner.
        owner: ProductionSimulationWorkerOwner,
    },
    /// Startup failed while preserving the owner for shutdown.
    Unavailable {
        /// Rejecting unified handoff.
        handoff: Arc<ProductionSimulationHandoff>,
        /// Sole worker owner.
        owner: ProductionSimulationWorkerOwner,
        /// Exact stable startup failure.
        error: Arc<ProductionSimulationInstallError>,
    },
}

impl ProductionStartup {
    /// Waits exactly once for worker readiness or typed failure.
    pub fn await_ready(self, timeout: Duration) -> ProductionInstallDisposition {
        let Self { handoff, owner, receiver, cancel } = self;
        match receiver.recv_timeout(timeout) {
            Ok(WorkerStartup::Ready { entrypoint, sender, admission }) => {
                let published = handoff.shared.state.lock().ok().and_then(|mut state| {
                    if matches!(&*state, ProductionHandoffState::Installing(_)) {
                        *state = ProductionHandoffState::Open { entrypoint, admission, sender };
                        Some(())
                    } else {
                        None
                    }
                });
                if published.is_some() {
                    ProductionInstallDisposition::Ready { handoff, owner }
                } else {
                    let error = Arc::new(ProductionSimulationInstallError::ActivationInvariant);
                    ProductionInstallDisposition::Unavailable { handoff, owner, error }
                }
            }
            Ok(WorkerStartup::Failed(error)) => {
                Self::set_unavailable(&handoff, Arc::clone(&error));
                ProductionInstallDisposition::Unavailable { handoff, owner, error }
            }
            Err(RecvTimeoutError::Timeout) => {
                cancel.store(true, Ordering::Release);
                let error = Arc::new(ProductionSimulationInstallError::WorkerStartupUnavailable(
                    WorkerStartupFailure::Timeout,
                ));
                Self::set_unavailable(&handoff, Arc::clone(&error));
                ProductionInstallDisposition::Unavailable { handoff, owner, error }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let error = Arc::new(ProductionSimulationInstallError::WorkerStartupUnavailable(
                    WorkerStartupFailure::SenderLost,
                ));
                Self::set_unavailable(&handoff, Arc::clone(&error));
                ProductionInstallDisposition::Unavailable { handoff, owner, error }
            }
        }
    }

    fn set_unavailable(
        handoff: &ProductionSimulationHandoff,
        error: Arc<ProductionSimulationInstallError>,
    ) {
        let Ok(mut state) = handoff.shared.state.lock() else {
            return;
        };
        if matches!(&*state, ProductionHandoffState::Installing(_)) {
            *state = ProductionHandoffState::Unavailable(error);
        }
    }
}

impl SimulationWorker {
    /// Spawns the sole named production simulation worker and one-shot startup channel.
    pub fn spawn<C, F>(inputs: ProductionInstallInputs<C, F>) -> ProductionSpawnDisposition
    where
        C: super::CommittedStateAuthority + std::fmt::Debug + Send + Sync + 'static,
        F: super::FinalizedChainAuthority + 'static,
    {
        let installer = ProductionHandoffInstaller::new();
        let handoff = installer.handoff();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_handoff = Arc::clone(&handoff);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let bootstrap = installer.into_worker(WorkerStartupSender(startup_sender));
        let thread =
            std::thread::Builder::new().name("base-mev-arm-egress".to_owned()).spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_production_worker(
                        inputs,
                        bootstrap,
                        Arc::clone(&worker_handoff),
                        Arc::clone(&worker_cancel),
                        SimulationEntrypoint::ready,
                    );
                }));
                if result.is_err() {
                    worker_handoff
                        .close(ProductionHandoffClosed::Worker(ProductionWorkerError::WorkerPanic));
                }
            });
        match thread {
            Ok(thread) => {
                let owner = ProductionSimulationWorkerOwner::new(
                    thread,
                    Arc::clone(&cancel),
                    Arc::clone(&handoff),
                );
                ProductionSpawnDisposition::Spawned(ProductionStartup {
                    handoff,
                    owner,
                    receiver: startup_receiver,
                    cancel,
                })
            }
            Err(_) => {
                let error = Arc::new(ProductionSimulationInstallError::WorkerSpawnUnavailable);
                ProductionStartup::set_unavailable(&handoff, Arc::clone(&error));
                ProductionSpawnDisposition::Unavailable { handoff, error }
            }
        }
    }
}

fn run_production_worker<C, F, R>(
    inputs: ProductionInstallInputs<C, F>,
    bootstrap: ProductionWorkerBootstrap,
    handoff: Arc<ProductionSimulationHandoff>,
    cancel: Arc<AtomicBool>,
    ready: R,
) where
    C: super::CommittedStateAuthority + std::fmt::Debug + Send + Sync + 'static,
    F: super::FinalizedChainAuthority + 'static,
    R: FnOnce() -> SimulationEntrypoint,
{
    let ProductionInstallInputs { bundle, armed } = inputs;
    let ProductionInstallBundle {
        committed_state,
        bridge,
        simulation_backend,
        proofs,
        claims,
        prepared_loss,
        mut store,
    } = bundle;
    if let Err(error) = store.ensure_capacity() {
        let reason =
            SimulationLedgerClosure::try_from(error).expect("capacity failure is a closure");
        let _ = bootstrap.publish_failed(Arc::new(
            ProductionSimulationInstallError::CapacityUnavailable(reason),
        ));
        return;
    }
    let settled = match prepared_loss.activate() {
        Ok(settled) => settled,
        Err(error) => {
            let _ = bootstrap.publish_failed(Arc::new(
                ProductionSimulationInstallError::DrawdownAuthorityUnavailable(error),
            ));
            return;
        }
    };
    let provider = ProductionCodeHashProvider::install(Arc::clone(&committed_state));
    let deployment = match proofs.verify_deployment(&provider) {
        Ok(deployment) => deployment,
        Err(_) => {
            let _ = bootstrap.publish_failed(Arc::new(
                ProductionSimulationInstallError::DeploymentIdentityUnavailable(
                    ProductionDeploymentFailure::ProviderUnavailable,
                ),
            ));
            return;
        }
    };
    let runtime = match ProductionB5Runtime::install(&deployment, &claims, committed_state, settled)
    {
        Ok(runtime) => runtime,
        Err(ProductionB5RuntimeInstallError::DeploymentIdentity(_)) => {
            let _ = bootstrap.publish_failed(Arc::new(
                ProductionSimulationInstallError::DeploymentIdentityUnavailable(
                    ProductionDeploymentFailure::BinaryMismatch,
                ),
            ));
            return;
        }
        Err(ProductionB5RuntimeInstallError::ArmRuntime(error)) => {
            let failure = match error {
                super::ArmRuntimeOpenError::Startup(_) => ProductionArmRuntimeOpenFailure::Startup,
                super::ArmRuntimeOpenError::Suppression(_) => {
                    ProductionArmRuntimeOpenFailure::Suppression
                }
            };
            let _ = bootstrap.publish_failed(Arc::new(
                ProductionSimulationInstallError::ArmRuntimeUnavailable(failure),
            ));
            return;
        }
    };
    let entrypoint = Arc::new(ready());
    let receiver = match bootstrap.publish_ready(Arc::clone(&entrypoint)) {
        Ok(receiver) => receiver,
        Err(WorkerStartup::Ready { .. }) | Err(WorkerStartup::Failed(_)) => return,
    };
    run_candidate_loop(
        &handoff,
        &cancel,
        || receiver.receive(),
        |admitted| {
            let (candidate, reservation) = admitted.into_parts();
            let terminal = process_candidate(
                candidate,
                &bridge,
                &proofs,
                &claims,
                &runtime,
                &armed,
                &simulation_backend,
                &mut store,
                &entrypoint,
            );
            drop(reservation);
            terminal
        },
    );
}

fn run_candidate_loop<T, R, P>(
    handoff: &Arc<ProductionSimulationHandoff>,
    cancel: &Arc<AtomicBool>,
    mut receive: R,
    mut process: P,
) where
    R: FnMut() -> Result<T, ()>,
    P: FnMut(T) -> Result<(), ProductionHandoffClosed>,
{
    loop {
        let admitted = match receive() {
            Ok(admitted) => admitted,
            Err(()) => {
                handoff.close(ProductionHandoffClosed::Disconnected);
                return;
            }
        };
        match process(admitted) {
            Ok(()) => {}
            Err(reason) if reason.is_candidate_denial() => {
                let _ = handoff.shared.denied_candidates.fetch_add(1, Ordering::AcqRel);
            }
            Err(reason) => {
                handoff.close(reason);
                return;
            }
        }
        if cancel.load(Ordering::Acquire) {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_candidate<C, F>(
    candidate: SealedUnsignedCandidate,
    bridge: &InstalledSubmissionBridge,
    proofs: &ProductionProofBundle,
    claims: &VictimClaimStore,
    runtime: &ProductionB5Runtime<C, NodeLocalSettledLossAuthority<Arc<F>>>,
    armed: &ArmedCriteria,
    simulation_backend: &SimBackend,
    store: &mut SimulationStore,
    entrypoint: &SimulationEntrypoint,
) -> Result<(), ProductionHandoffClosed>
where
    C: super::CommittedStateAuthority,
    F: super::FinalizedChainAuthority,
{
    let checked = bridge
        .into_checked_candidate(candidate, proofs.campaign_id(), runtime.code_hash_provider())
        .map_err(|error| {
            ProductionHandoffClosed::Worker(ProductionWorkerError::Bridge(map_bridge_failure(
                error,
            )))
        })?;
    let verified = proofs
        .verify_candidate(runtime.code_hash_provider())
        .map_err(ProductionHandoffClosed::Authorization)?;
    if let Err(error) = store.ensure_capacity() {
        let reason =
            SimulationLedgerClosure::try_from(error).expect("capacity failure is a closure");
        entrypoint.close_ledger(reason);
        return Err(ProductionHandoffClosed::LedgerClosed);
    }
    let identity = checked.execution_identity();
    let claim = match try_claim_detailed(
        claims,
        CHAIN_ID_BASE,
        identity.victim(),
        identity.campaign_id(),
        runtime.sink(),
    ) {
        Ok(ProductionClaimResult::Claimed(claim)) => claim,
        Ok(ProductionClaimResult::AlreadyClaimed) => {
            return Err(ProductionHandoffClosed::Worker(
                ProductionWorkerError::ClaimAlreadyClaimed,
            ));
        }
        Err(error) => {
            return Err(ProductionHandoffClosed::Worker(ProductionWorkerError::ClaimError {
                source: error.source(),
                latch: error.latch(),
            }));
        }
    };
    let authorized = runtime
        .authorize(armed, verified, claim, checked)
        .map_err(ProductionHandoffClosed::Authorization)?;
    let signed = authorized
        .load_and_sign_detailed(runtime.sink())
        .map_err(|error| ProductionHandoffClosed::Worker(ProductionWorkerError::Signing(error)))?;
    let attempt = super::SubmissionAttempt::Initial(super::PairedSubmission::assemble(signed));
    let freshness = runtime.freshness(armed);
    let record =
        match send_gated(attempt, &freshness, RuntimeBackend::simulated(simulation_backend)) {
            SubmitOutcome::Simulated(record) => record,
            SubmitOutcome::NoEgress
            | SubmitOutcome::LiveLocksClosed(_)
            | SubmitOutcome::InclusionFailed(_)
            | SubmitOutcome::InclusionSentAttributionFailed { .. }
            | SubmitOutcome::LiveComplete => {
                return Err(ProductionHandoffClosed::Worker(
                    ProductionWorkerError::UnexpectedLiveOutcome,
                ));
            }
        };
    let persisted = match store.append(&record) {
        Ok(persisted) => persisted,
        Err(
            error @ (SimulationPersistError::Full { .. }
            | SimulationPersistError::WriteFailed { .. }),
        ) => {
            let reason = SimulationLedgerClosure::try_from(error).expect("persistence closure");
            entrypoint.close_ledger(reason);
            return Err(ProductionHandoffClosed::LedgerClosed);
        }
        Err(SimulationPersistError::MissingEconomics) => {
            return Err(ProductionHandoffClosed::Worker(ProductionWorkerError::Persistence(
                ProductionPersistenceFailure::MissingEconomics,
            )));
        }
        Err(SimulationPersistError::MissingIdentityEvidence) => {
            return Err(ProductionHandoffClosed::Worker(ProductionWorkerError::Persistence(
                ProductionPersistenceFailure::MissingIdentityEvidence,
            )));
        }
        Err(SimulationPersistError::Oversize) => {
            return Err(ProductionHandoffClosed::Worker(ProductionWorkerError::Persistence(
                ProductionPersistenceFailure::Oversize,
            )));
        }
    };
    if persisted.ledger_full_after_commit() {
        let reason = SimulationLedgerClosure::Full {
            ledger_epoch: persisted.correlation().ledger_epoch(),
            next_sequence: SIMULATION_RECORD_CAPACITY,
            capacity: SIMULATION_RECORD_CAPACITY,
        };
        entrypoint.close_ledger(reason);
        return Err(ProductionHandoffClosed::LedgerClosed);
    }
    Ok(())
}

fn map_bridge_failure(error: BridgeError) -> ProductionBridgeFailure {
    match error {
        BridgeError::Assembly(error) => ProductionBridgeFailure::Assembly(error),
        BridgeError::BindingRejected => ProductionBridgeFailure::BindingRejected,
        BridgeError::CrossInstallation => ProductionBridgeFailure::CrossInstallation,
        BridgeError::SnapshotStale => ProductionBridgeFailure::SnapshotStale,
        BridgeError::ExecutionFreshnessUnavailable => ProductionBridgeFailure::FreshnessUnavailable,
        BridgeError::ExecutionIdentityChanged => ProductionBridgeFailure::IdentityChanged,
        BridgeError::Cancelled => ProductionBridgeFailure::Cancelled,
        BridgeError::DeadlineNoHandoff => ProductionBridgeFailure::Deadline,
    }
}
fn map_claim_failure(error: ClaimStoreError) -> ProductionClaimFailure {
    match error {
        ClaimStoreError::Io(_) => ProductionClaimFailure::Io,
        ClaimStoreError::Corruption(_) => ProductionClaimFailure::Corruption,
        ClaimStoreError::CommitFailed(_) => ProductionClaimFailure::CommitUnknown,
        ClaimStoreError::NotSingletonWriter => ProductionClaimFailure::NotSingleton,
        ClaimStoreError::StoreIdentityMismatch => ProductionClaimFailure::IdentityMismatch,
    }
}
#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    enum StartupMode {
        Ready,
        SenderLost,
        Timeout,
    }

    fn startup(mode: StartupMode) -> ProductionStartup {
        let installer = ProductionHandoffInstaller::new();
        let handoff = installer.handoff();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let bootstrap = installer.into_worker(WorkerStartupSender(startup_sender));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let thread = thread::Builder::new()
            .name("base-mev-arm-egress".to_owned())
            .spawn(move || match mode {
                StartupMode::Ready => {
                    let receiver = bootstrap
                        .publish_ready(Arc::new(SimulationEntrypoint::ready()))
                        .expect("ready acknowledgement");
                    while !worker_cancel.load(Ordering::Acquire) && receiver.receive().is_ok() {}
                }
                StartupMode::SenderLost => drop(bootstrap),
                StartupMode::Timeout => {
                    thread::sleep(Duration::from_millis(50));
                    let _ = bootstrap.publish_ready(Arc::new(SimulationEntrypoint::ready()));
                }
            })
            .expect("spawn startup worker");
        let owner =
            ProductionSimulationWorkerOwner::new(thread, Arc::clone(&cancel), Arc::clone(&handoff));
        ProductionStartup { handoff, owner, receiver: startup_receiver, cancel }
    }

    #[test]
    fn real_candidate_loop_continues_from_denial_to_success() {
        let entrypoint = Arc::new(SimulationEntrypoint::ready());
        let admission = Arc::new(AtomicU8::new(ADMISSION_FREE));
        let (handoff_sender, _handoff_receiver) = sync_channel(1);
        let handoff =
            Arc::new(ProductionSimulationHandoff::install(ProductionHandoffState::Open {
                entrypoint,
                admission,
                sender: handoff_sender,
            }));
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = sync_channel(6);
        for candidate in 0_u8..=5 {
            sender.send(candidate).expect("queued candidate");
        }
        drop(sender);

        let mut processed = Vec::new();
        run_candidate_loop(
            &handoff,
            &cancel,
            || receiver.recv().map_err(|_| ()),
            |candidate| {
                processed.push(candidate);
                let denial = match candidate {
                    0 => Some(ProductionHandoffClosed::Worker(
                        ProductionWorkerError::ClaimAlreadyClaimed,
                    )),
                    1 => Some(ProductionHandoffClosed::Authorization(
                        ProductionCandidateError::CampaignMismatch,
                    )),
                    2 => Some(ProductionHandoffClosed::Worker(ProductionWorkerError::Bridge(
                        ProductionBridgeFailure::SnapshotStale,
                    ))),
                    3 => Some(ProductionHandoffClosed::Worker(ProductionWorkerError::Bridge(
                        ProductionBridgeFailure::Deadline,
                    ))),
                    4 => Some(ProductionHandoffClosed::Worker(ProductionWorkerError::Bridge(
                        ProductionBridgeFailure::Cancelled,
                    ))),
                    _ => None,
                };
                match denial {
                    Some(denial) => Err(denial),
                    None => {
                        cancel.store(true, Ordering::Release);
                        Ok(())
                    }
                }
            },
        );

        assert_eq!(processed, [0, 1, 2, 3, 4, 5]);
        assert_eq!(handoff.candidate_denial_count(), 5);
        assert_eq!(handoff.status(), ProductionSimulationHandoffStatus::Ready);
    }

    #[test]
    fn startup_ready_publishes_once_and_owner_joins() {
        let disposition = startup(StartupMode::Ready).await_ready(Duration::from_secs(5));
        let ProductionInstallDisposition::Ready { handoff, owner } = disposition else {
            panic!("expected ready disposition");
        };
        assert_eq!(handoff.status(), ProductionSimulationHandoffStatus::Ready);
        owner.into_join_handle().join().expect("worker join");
    }

    #[test]
    fn startup_sender_loss_is_typed_and_rejecting() {
        let disposition = startup(StartupMode::SenderLost).await_ready(Duration::from_secs(5));
        let ProductionInstallDisposition::Unavailable { handoff, owner, error } = disposition
        else {
            panic!("expected unavailable disposition");
        };
        assert_eq!(
            *error,
            ProductionSimulationInstallError::WorkerStartupUnavailable(
                WorkerStartupFailure::SenderLost,
            )
        );
        assert_eq!(
            handoff.status(),
            ProductionSimulationHandoffStatus::Unavailable(Arc::clone(&error))
        );
        owner.into_join_handle().join().expect("worker join");
    }

    #[test]
    fn startup_timeout_cancels_late_ready_and_stays_unavailable() {
        let disposition = startup(StartupMode::Timeout).await_ready(Duration::from_millis(5));
        let ProductionInstallDisposition::Unavailable { handoff, owner, error } = disposition
        else {
            panic!("expected unavailable disposition");
        };
        assert_eq!(
            *error,
            ProductionSimulationInstallError::WorkerStartupUnavailable(
                WorkerStartupFailure::Timeout,
            )
        );
        owner.into_join_handle().join().expect("worker join");
        assert_eq!(handoff.status(), ProductionSimulationHandoffStatus::Unavailable(error));
    }
}
