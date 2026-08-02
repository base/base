#![doc = "Feature-gated, fail-closed MEV transaction authority and submission tiers."]
#![forbid(unsafe_code)]

// The default build resolves none of the optional submit dependencies. T4b
// unsigned authority is separate from the phase-b signer and arm tiers.
#[cfg(feature = "phase-b")]
pub mod assembler;
#[cfg(feature = "tx-authority")]
mod canonical_envelope;
#[cfg(feature = "tx-authority")]
pub use canonical_envelope::{
    CanonicalEnvelopeError, CanonicalEnvelopeFactory, CanonicalEnvelopeFeeEvidence,
    CanonicalEnvelopeOwner, CanonicalL1EnvelopeEvidence, MAX_CANONICAL_ENVELOPE_LEN,
};
#[cfg(feature = "tx-authority")]
mod calldata;
#[cfg(any(feature = "phase-b", feature = "tx-authority"))]
mod economics;
#[cfg(feature = "tx-authority")]
pub mod fee;
#[cfg(feature = "tx-authority")]
pub use economics::PriorityEconomicsAuthority;
#[cfg(feature = "tx-authority")]
pub use economics::PriorityEconomicsReceipt;
#[cfg(feature = "phase-b")]
pub mod signer;

#[cfg(feature = "tx-authority")]
mod tx_authority;
#[cfg(feature = "t4d-bridge")]
pub use tx_authority::{
    AdapterAwareProofBindings, BridgeError, InstalledSubmissionBridge, SealedUnsignedCandidate,
};
#[cfg(feature = "t4e-handoff")]
pub use tx_authority::{BridgeConversionSeal, T4eCandidateHandoff, T4eHandoffError};
#[cfg(feature = "tx-authority")]
pub use tx_authority::{
    CandidateEconomicsEvidence, CandidateExecutionAdapter, CheckedBerylEnvInputs, CheckedBindings,
    CheckedBindingsView, DeployedContractIdentity, DeploymentWitness, EconomicsReadyCandidate,
    ExecuteOnceError, FreshnessWitness, InstalledExecutionIdentity, NonceWitness,
    PreEconomicsCandidate, ProtocolAdapterMapping, SnapshotFreshnessToken, TxAuthorityAssembler,
    TxAuthorityError, TxAuthorityExecutionParts, TxAuthorityExecutionRequest, TxAuthorityNodeError,
    TxAuthorityNodeView, TxAuthorityStateRead, UnsignedTxShapeObservation,
    ValidatedUnsignedAtomicTx,
};

// B3-arm tier remains private. The root exposes only reviewed handoff/provider,
// provisioning, and S1-b runtime-backend selection types under their owning
// features; raw permits, signing, custody, proof construction, and send stay private.
#[cfg(feature = "arm")]
mod arm;
#[cfg(all(feature = "arm-live-egress", not(test)))]
pub use arm::ProdBackend;
#[cfg(feature = "t4e-handoff")]
pub use arm::{
    AdmittedCandidate, ProductionArmFailure, ProductionArmRuntimeOpenFailure,
    ProductionBridgeFailure, ProductionBundleInputs, ProductionCampaignBundleFailure,
    ProductionCandidateReceiver, ProductionDeploymentFailure, ProductionHandoffClosed,
    ProductionHandoffInstaller, ProductionHandoffShared, ProductionHandoffState,
    ProductionInstallBundle, ProductionInstallDisposition, ProductionInstallInputs,
    ProductionPersistenceFailure, ProductionProofBundle, ProductionProviderFailure,
    ProductionSimulationHandoff, ProductionSimulationHandoffStatus,
    ProductionSimulationInstallError, ProductionSimulationWorkerOwner, ProductionSpawnDisposition,
    ProductionStartup, ProductionStoreOpenFailure, ProductionWorkerBootstrap,
    ProductionWorkerError, SimulationWorker, VerifiedProductionProofs, WorkerStartup,
    WorkerStartupFailure,
};
#[cfg(feature = "arm")]
pub use arm::{
    AuthorizationGateError, BlockNumHash, BoundedSubmissionIdV1, BoundedUnresolvedSummaryV1,
    CanonicalDeploymentPairV1, CanonicalG7PairV1, CanonicalLivePairV1, CanonicalMismatchClass,
    FinalizedChainAuthority, FinalizedChainError, FrozenP2PopulationManifestV1,
    NodeLocalSettledLossAuthority, PopulationClosureFieldsV1, PopulationKindV1,
    PreparedSettledLossAuthority, ProducerConformance, ProducerError, ProductionCandidateError,
    ProductionClaimError, ProductionClaimFailure, ProductionClaimResult, ProductionCustodyFailure,
    ProductionDrawdownSource, ProductionLatchOutcome, ProductionSignFailure, ProductionSignedField,
    ProductionSigningError, ProjectionClosureFieldsV1, PublicationIoClass,
    PublishedPopulationManifestV1, RuntimeBackend, SETTLED_LOSS_ANCHOR_PATH,
    SETTLED_LOSS_PROJECTION_PATH, SettledLossLoad, SettledLossReader, SettledLossUnavailableReason,
    SignedInstallBundleV1, SignedPopulationManifestV1, SignedProjectionV1, SimBackend,
    SimulationCorrelationEnvelopeV1, SimulationCorrelationKey, SimulationEntrypointStatus,
    SimulationEntrypointUnavailable, SimulationLedgerClosure, SimulationLedgerEpoch,
    SimulationLedgerInvalid, SimulationReservation, SimulationStoreOperation, SourceLedgerRowV1,
    SourceSubmissionManifestEntryV1, TerminalKindV1, TerminalSettlementEntryV1,
    TerminalSettlementProjectionV1, UnresolvedReasonV1, UnsignedInstallBundleV1,
    UnsignedPopulationManifestV1, UnsignedProjectionV1, production_custody_preflight,
    try_claim_detailed,
};
#[cfg(feature = "t4e-handoff")]
pub use arm::{CheckedCandidate, CodeHashProvider, CommittedStateAuthority, ProviderError};
#[cfg(all(feature = "arm", feature = "arm-provisioning"))]
pub use arm::{
    ParsedFrozenExportV2, ProvisioningToolError, SuppressionRollbackError, T4eProvisioningTool,
    provision_suppression_anchor,
};

// B5-1a `presign` dormant tier. The module is PRIVATE; only the reviewed value/
// digest surface is re-exported. It is pure and provider/filesystem/environment/
// receipt/constant/global/callback-free, and everything it exports is forgeable
// and non-authorizing — the CLI-private Commit-B verifier (not this crate) owns
// every external authority comparison. No phase-b/arm item is reachable from it.
#[cfg(feature = "presign")]
mod dormant;
#[cfg(feature = "presign")]
pub use dormant::{
    AuthenticatedProvisioningSnapshot, B5_DEPLOYMENT_REVIEW_DOMAIN, B5_DORMANT_PROVISIONING_DOMAIN,
    B5_PROVISIONING_VALUE_SET_DOMAIN, DigestFramingError, DomainSeparatedSha256,
    ProvisioningSnapshotError,
};

/// The Blink OFA native-ETH kickback recipient enforced inside the executor
/// backrun. Mirrors `BLINK_OFA_KICKBACK_RECIPIENT` in the TS prototype and the
/// `NATIVE_KICKBACK_RECIPIENT` constant compiled into `BlinkAtomicExecutor`.
#[cfg(feature = "phase-b")]
pub const BLINK_OFA_KICKBACK_RECIPIENT: alloy_primitives::Address =
    alloy_primitives::address!("743be0db30148336a3db479f19d4e1828b293869");

/// The minimum kickback share (basis points) the executor pays to the recipient.
/// Mirrors `BLINK_OFA_MIN_KICKBACK_BPS` in the TS prototype; the executor pays
/// `ceil(75%)` of realized profit, i.e. at least this share.
#[cfg(any(feature = "phase-b", feature = "tx-authority"))]
pub const BLINK_OFA_MIN_KICKBACK_BPS: u32 = 7_500;
