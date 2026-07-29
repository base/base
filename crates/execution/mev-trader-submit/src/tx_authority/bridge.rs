//! Opaque unsigned handoff scaffolding bound to one installed node authority.

use std::{
    fmt::{self, Debug},
    sync::Arc,
    time::Instant,
};

#[cfg(feature = "t4e-handoff")]
use crate::CheckedCandidate;
use alloy_primitives::{Address, B256};
#[cfg(feature = "t4e-handoff")]
use base_mev_trader::CampaignId;
use base_mev_trader::{
    CancellationProbe, CandidateAssemblyView, ExactProtocol, GlobalState, MeasurementContext,
    TaskState,
};

use super::{
    DeployedContractIdentity, InstalledExecutionIdentity, TxAuthorityAssembler, TxAuthorityError,
    TxAuthorityNodeView, ValidatedUnsignedAtomicTx,
};

/// Non-authorizing structural bindings retained by an opaque unsigned candidate.
#[derive(Debug, PartialEq, Eq)]
pub struct AdapterAwareProofBindings {
    frame: MeasurementContext,
    victim: B256,
    plan_digest: B256,
    sender: Address,
    nonce: u64,
    valid_until_block: u64,
    unsigned_signing_hash: B256,
    validated_parent: B256,
    executor: DeployedContractIdentity,
    route_protocols: [ExactProtocol; 2],
    route_adapters: [DeployedContractIdentity; 2],
}

impl AdapterAwareProofBindings {
    /// Returns the full measurement frame identity.
    pub const fn frame(&self) -> MeasurementContext {
        self.frame
    }

    /// Returns the bound victim transaction hash.
    pub const fn victim(&self) -> B256 {
        self.victim
    }

    /// Returns the bound measurement plan digest.
    pub const fn plan_digest(&self) -> B256 {
        self.plan_digest
    }

    /// Returns the installed public sender address.
    pub const fn sender(&self) -> Address {
        self.sender
    }

    /// Returns the snapshot-derived unsigned nonce.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Returns the candidate's last valid block.
    pub const fn valid_until_block(&self) -> u64 {
        self.valid_until_block
    }

    /// Returns the unsigned transaction signing hash.
    pub const fn unsigned_signing_hash(&self) -> B256 {
        self.unsigned_signing_hash
    }

    /// Returns the committed parent used to validate execution identities.
    pub const fn validated_parent(&self) -> B256 {
        self.validated_parent
    }

    /// Returns the installed executor address and runtime hash.
    pub const fn executor(&self) -> &DeployedContractIdentity {
        &self.executor
    }

    /// Returns the exact two-hop protocol order.
    pub const fn route_protocols(&self) -> [ExactProtocol; 2] {
        self.route_protocols
    }

    /// Returns installed adapter identities in exact two-hop route order.
    pub const fn route_adapters(&self) -> [&DeployedContractIdentity; 2] {
        [&self.route_adapters[0], &self.route_adapters[1]]
    }
}

/// Linear unsigned candidate sealed to one installed submission bridge.
pub struct SealedUnsignedCandidate {
    detail: ValidatedUnsignedAtomicTx,
    bindings: AdapterAwareProofBindings,
    installation: Arc<InstallationSeal>,
    probe: CancellationProbe,
}

impl SealedUnsignedCandidate {
    /// Returns only the bounded, non-authorizing structural bindings.
    pub const fn bindings(&self) -> &AdapterAwareProofBindings {
        &self.bindings
    }
}

impl Debug for SealedUnsignedCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedUnsignedCandidate")
            .field("bindings", &self.bindings)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct InstallationSeal;

/// Unforgeable capability proving that a T4d candidate passed bridge revalidation.
#[cfg(feature = "t4e-handoff")]
#[derive(Debug)]
pub struct BridgeConversionSeal {
    private: (),
}

#[cfg(feature = "t4e-handoff")]
impl BridgeConversionSeal {
    pub(crate) fn consume(self) {
        let _ = self.private;
    }
}

/// Terminal failure from an unsigned T4e candidate handoff sink.
#[cfg(feature = "t4e-handoff")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum T4eHandoffError {
    /// The bounded sink already owns a candidate.
    Busy,
    /// The sink no longer accepts candidates.
    Closed,
    /// The sink rejected the candidate without retry.
    Rejected,
}

/// Synchronous by-value boundary from T4d drain to the owner-supplied T4e consumer.
#[cfg(feature = "t4e-handoff")]
pub trait T4eCandidateHandoff: Debug + Send + Sync {
    /// Consumes exactly one sealed candidate; errors are terminal and never return it.
    fn try_handoff(&self, candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError>;
}

/// Submit-owned facade that keeps assembly and freshness on one node authority.
#[derive(Debug)]
pub struct InstalledSubmissionBridge {
    assembler: TxAuthorityAssembler,
    node: Arc<dyn TxAuthorityNodeView>,
    installation: Arc<InstallationSeal>,
}

impl InstalledSubmissionBridge {
    /// Installs the reviewed Base-mainnet identities against one owned node view.
    pub fn base_mainnet(node: Arc<dyn TxAuthorityNodeView>) -> Result<Self, BridgeError> {
        let assembler =
            TxAuthorityAssembler::base_mainnet(Arc::clone(&node)).map_err(BridgeError::Assembly)?;
        Ok(Self { assembler, node, installation: Arc::new(InstallationSeal) })
    }

    #[cfg(test)]
    pub(super) fn install_for_test(
        node: Arc<dyn TxAuthorityNodeView>,
        executor: DeployedContractIdentity,
        sender: Address,
        adapters: super::ProtocolAdapterMapping,
    ) -> Result<Self, BridgeError> {
        let assembler =
            TxAuthorityAssembler::install(Arc::clone(&node), executor, sender, adapters)
                .map_err(BridgeError::Assembly)?;
        Ok(Self { assembler, node, installation: Arc::new(InstallationSeal) })
    }

    /// Assembles and seals one linear unsigned candidate without exposing transaction bytes.
    pub fn assemble_sealed(
        &self,
        view: CandidateAssemblyView<'_>,
    ) -> Result<SealedUnsignedCandidate, BridgeError> {
        let probe = view.probe().clone();
        let detail = self.assembler.assemble_validated(view).map_err(Self::assembly_error)?;
        self.seal_unsigned(detail, probe)
    }

    #[cfg(test)]
    pub(super) fn assemble_sealed_for_test(
        &self,
        view: super::AuthorityAssemblyView<'_>,
    ) -> Result<SealedUnsignedCandidate, BridgeError> {
        let probe = view.probe.clone();
        let detail = self.assembler.assemble_view(view).map_err(Self::assembly_error)?;
        self.seal_unsigned(detail, probe)
    }

    /// Consumes a sealed T4d candidate into the arm witness after one fresh bridge check.
    #[cfg(feature = "t4e-handoff")]
    pub fn into_checked_candidate(
        &self,
        candidate: SealedUnsignedCandidate,
        campaign_id: CampaignId,
    ) -> Result<CheckedCandidate, BridgeError> {
        self.revalidate_for_handoff(&candidate)?;
        let SealedUnsignedCandidate { detail, bindings: _, installation: _, probe: _ } = candidate;
        Ok(CheckedCandidate::from_authority(
            detail,
            campaign_id,
            BridgeConversionSeal { private: () },
        ))
    }
    /// Revalidates a sealed candidate for an immediate opaque handoff observation.
    pub fn revalidate_for_handoff<'a>(
        &self,
        candidate: &'a SealedUnsignedCandidate,
    ) -> Result<&'a AdapterAwareProofBindings, BridgeError> {
        Self::checkpoint(&candidate.probe)?;
        if !Arc::ptr_eq(&self.installation, &candidate.installation) {
            return Err(BridgeError::CrossInstallation);
        }
        candidate.detail.validate_at_drain().map_err(|_| BridgeError::SnapshotStale)?;

        let bindings = &candidate.bindings;
        let execution = candidate.detail.execution();
        let parent = self
            .node
            .current_parent_hash()
            .map_err(|_| BridgeError::ExecutionFreshnessUnavailable)?;
        if parent != bindings.validated_parent {
            return Err(BridgeError::ExecutionIdentityChanged);
        }
        let state = self
            .node
            .read_state_at_parent(
                parent,
                execution.sender(),
                TxAuthorityAssembler::contract_addresses(
                    execution.executor(),
                    execution.adapters(),
                ),
            )
            .map_err(|_| BridgeError::ExecutionFreshnessUnavailable)?;
        TxAuthorityAssembler::validate_state_codes(
            &state,
            parent,
            execution.executor(),
            execution.adapters(),
        )
        .map_err(|_| BridgeError::ExecutionIdentityChanged)?;
        if self
            .node
            .current_parent_hash()
            .map_err(|_| BridgeError::ExecutionFreshnessUnavailable)?
            != parent
        {
            return Err(BridgeError::ExecutionIdentityChanged);
        }
        Self::validate_bindings(bindings, execution)?;
        if state.parent_number() >= bindings.valid_until_block {
            return Err(BridgeError::DeadlineNoHandoff);
        }
        Self::checkpoint(&candidate.probe)?;
        Ok(bindings)
    }

    fn seal_unsigned(
        &self,
        detail: ValidatedUnsignedAtomicTx,
        probe: CancellationProbe,
    ) -> Result<SealedUnsignedCandidate, BridgeError> {
        let observation = detail.observation();
        let execution = detail.execution();
        let route_protocols = observation.hop_protocols();
        let expected_route = route_protocols.map(|protocol| execution.adapters().resolve(protocol));
        if observation.frame().parent_hash != execution.validated_parent()
            || observation.sender() != execution.sender()
            || observation.executor() != execution.executor().address()
            || observation.hop_adapters() != expected_route.map(DeployedContractIdentity::address)
            || observation.hop_runtime_hashes()
                != expected_route.map(DeployedContractIdentity::runtime_hash)
        {
            return Err(BridgeError::BindingRejected);
        }
        let bindings = AdapterAwareProofBindings {
            frame: observation.frame(),
            victim: observation.victim(),
            plan_digest: observation.plan_digest(),
            sender: observation.sender(),
            nonce: observation.nonce(),
            valid_until_block: observation.valid_until_block(),
            unsigned_signing_hash: observation.unsigned_signing_hash(),
            validated_parent: execution.validated_parent(),
            executor: execution.executor().clone(),
            route_protocols,
            route_adapters: route_protocols
                .map(|protocol| execution.adapters().resolve(protocol).clone()),
        };
        Self::validate_bindings(&bindings, execution)?;
        Ok(SealedUnsignedCandidate {
            detail,
            bindings,
            installation: Arc::clone(&self.installation),
            probe,
        })
    }

    fn validate_bindings(
        bindings: &AdapterAwareProofBindings,
        execution: &InstalledExecutionIdentity,
    ) -> Result<(), BridgeError> {
        let adapters = execution.adapters();
        let expected_route = bindings.route_protocols.map(|protocol| adapters.resolve(protocol));
        if bindings.frame.parent_hash != execution.validated_parent()
            || bindings.validated_parent != execution.validated_parent()
            || bindings.sender != execution.sender()
            || &bindings.executor != execution.executor()
            || bindings.route_adapters[0] != *expected_route[0]
            || bindings.route_adapters[1] != *expected_route[1]
        {
            return Err(BridgeError::BindingRejected);
        }
        Ok(())
    }

    const fn assembly_error(error: TxAuthorityError) -> BridgeError {
        match error {
            TxAuthorityError::Cancelled => BridgeError::Cancelled,
            TxAuthorityError::DeadlineNoShape => BridgeError::DeadlineNoHandoff,
            error => BridgeError::Assembly(error),
        }
    }

    fn checkpoint(probe: &CancellationProbe) -> Result<(), BridgeError> {
        let now = Instant::now();
        if now >= probe.token().deadline() {
            probe.token().request_cancel();
            probe.acknowledge_drop();
            return Err(BridgeError::DeadlineNoHandoff);
        }
        match probe.token().state() {
            TaskState::Active if probe.checkpoint(now, true) => Ok(()),
            TaskState::Completed if probe.global().state() == GlobalState::Running => Ok(()),
            TaskState::Active | TaskState::CancelRequested | TaskState::DroppedAcked => {
                probe.acknowledge_drop();
                Err(BridgeError::Cancelled)
            }
            TaskState::Completed => Err(BridgeError::Cancelled),
        }
    }
}

/// Fail-closed reason that no sealed unsigned handoff observation was produced.
#[derive(Debug, PartialEq, Eq)]
pub enum BridgeError {
    /// Existing unsigned candidate assembly failed.
    Assembly(TxAuthorityError),
    /// Structural bindings disagreed with retained installed execution identity.
    BindingRejected,
    /// Candidate and facade were created by different installations.
    CrossInstallation,
    /// The exact captured pending snapshot is no longer authoritative.
    SnapshotStale,
    /// The owned node view could not provide execution freshness.
    ExecutionFreshnessUnavailable,
    /// Executor, adapter, or committed-parent identity changed.
    ExecutionIdentityChanged,
    /// The stored lifecycle probe was cancelled.
    Cancelled,
    /// The stored lifecycle deadline elapsed before handoff.
    DeadlineNoHandoff,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Assembly(error) => {
                write!(formatter, "unsigned candidate assembly failed: {error}")
            }
            Self::BindingRejected => formatter.write_str("structural bindings rejected"),
            Self::CrossInstallation => formatter.write_str("cross-installation candidate rejected"),
            Self::SnapshotStale => formatter.write_str("captured snapshot is stale"),
            Self::ExecutionFreshnessUnavailable => {
                formatter.write_str("execution freshness unavailable")
            }
            Self::ExecutionIdentityChanged => formatter.write_str("execution identity changed"),
            Self::Cancelled => formatter.write_str("handoff cancelled"),
            Self::DeadlineNoHandoff => formatter.write_str("handoff deadline elapsed"),
        }
    }
}

impl std::error::Error for BridgeError {}
