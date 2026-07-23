#![doc = "Feature-gated, fail-closed MEV transaction authority and submission tiers."]
#![forbid(unsafe_code)]

// The default build resolves none of the optional submit dependencies. T4b
// unsigned authority is separate from the phase-b signer and arm tiers.
#[cfg(feature = "phase-b")]
pub mod assembler;
#[cfg(feature = "tx-authority")]
mod calldata;
#[cfg(feature = "tx-authority")]
pub mod fee;
#[cfg(feature = "phase-b")]
pub mod signer;

#[cfg(feature = "tx-authority")]
mod tx_authority;
#[cfg(feature = "tx-authority")]
pub use tx_authority::{
    DeployedContractIdentity, InstalledExecutionIdentity, ProtocolAdapterMapping,
    SnapshotFreshnessToken, TxAuthorityAssembler, TxAuthorityError, TxAuthorityNodeError,
    TxAuthorityNodeView, TxAuthorityStateRead, UnsignedTxShapeObservation,
    ValidatedUnsignedAtomicTx,
};

// B3-arm tier — the real key loader + signer core + proof witness + transport
// builders. Entered through this single line and gated behind `arm`; the default
// and `phase-b` builds never compile it.
//
// B3-arm tier. The module is PRIVATE; only a curated forward-B5 surface is
// re-exported (facade). This is the public API a separately approved, owner-gated
// B5 arm linkage could invoke, while every low-level injection point (arbitrary
// store paths, fixture source impls, request builders, custody loaders) stays
// crate-private. The selected B5-1a dormant node feature does not enable or link
// this arm tier.
#[cfg(feature = "arm")]
mod arm;
#[cfg(feature = "arm")]
pub use arm::Channel;
#[cfg(all(feature = "arm", feature = "arm-live-egress", not(test)))]
pub use arm::ProdBackend;
#[cfg(all(feature = "arm", feature = "arm-provisioning"))]
pub use arm::provision_suppression_anchor;
#[cfg(feature = "arm")]
pub use arm::{
    ArmError, ArmRuntime, ArmRuntimeOpenError, ArmedFailSink, AttributionRetryToken,
    AuthorizedCandidate, AuthorizedSignedSubmission, CHAIN_ID_BASE, CheckedCandidate,
    CodeHashProvider, DeploymentEvidence, DeploymentIdentity, DeploymentIdentitySource,
    DeploymentPayload, DrawdownSource, EgressPlan, FreshnessSources, G7Attestation, G7Payload,
    LiveRunAttestation, LiveRunPayload, PairedSubmission, ProofBindings, ProviderError, RawBackend,
    RawEgress, RequestSpec, SubmissionAttempt, SubmitOutcome, SubmitSuppressionClear,
    SuppressionRollbackError, ValidatedExecutionIdentity, send_gated, try_claim_arm,
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
#[cfg(feature = "phase-b")]
pub const BLINK_OFA_MIN_KICKBACK_BPS: u32 = 7_500;
