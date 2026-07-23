//! Phase-B in-node MEV submit safe-prefix (rung-1 + rung-2, funds-0, dry-run).
//!
//! This crate is the Rust in-node port of the `TypeScript` verification prototypes
//! `scripts/arb-dryrun/blink-unsigned-assembler.ts` (rung-1) and
//! `scripts/arb-dryrun/rung2-ephemeral-signer.ts` (rung-2). It consumes the
//! measurement-only [`base_mev_trader::BackrunPlan`] and produces:
//!
//! * **rung-1** ([`assembler`]) — the executor calldata for
//!   `BlinkAtomicExecutor.executeBlinkOfaAtomic`, an unsigned EIP-1559 backrun
//!   envelope, a structurally-invalid dummy-signature serialization, and the
//!   two Blink OFA channel structures (inclusion + attribution). Assembly and
//!   serialization ONLY — nothing is ever transmitted.
//! * **rung-2** ([`signer`]) — a throwaway, in-memory, unfunded ephemeral k256
//!   keypair that signs the rung-1 envelope once and is verified entirely
//!   offline (ecrecover + field integrity). No key is ever loaded, persisted,
//!   logged, or returned.
//!
//! ## Red-line (compile-time enforced)
//!
//! The default build compiles an empty lib: no dependency is resolved and no
//! signer/submit code exists. The separate, default-off `presign` tier may be
//! linked by the selected dormant node feature, but resolves only
//! `alloy-primitives` and `sha2`; it exposes no assembler, signer, request,
//! transport, or egress path. The `phase-b`/`arm` trees remain absent unless
//! their own features are explicitly selected.
//!
//! The default, `phase-b`, and `presign` tiers contain no persistent-key loader
//! or real submission sink; the phase-b e2e uses only a spawned loopback anvil.
//! The separately gated `arm` tier owns the real key loader and signer core,
//! while only `arm-live-egress` compiles the real network sink.
//!
//! The B5-1a `presign` feature is a separate, dormant tier: it compiles ONLY
//! the pure, provider-free [`dormant`] value/digest surface (direct
//! dependencies exactly `alloy-primitives` + `sha2`) and none of the phase-b/
//! arm signer, assembler, candidate, or transport surface.

// Crate-wide: no module (arm or otherwise) may forge an `unsafe` block. This is a
// stronger seal than a per-module attribute — a sibling module cannot re-enable it.
#![forbid(unsafe_code)]

// Without `phase-b` none of the phase-b submit surface below exists. The gate
// lives on each item (not on the crate root) so the dormant B5-1a `presign` tier
// can compile without resolving any phase-b/arm dependency. The no-feature
// default remains an empty lib with zero signer/submit code.
#[cfg(feature = "phase-b")]
pub mod assembler;
#[cfg(feature = "phase-b")]
pub mod fee;
#[cfg(feature = "phase-b")]
pub mod signer;

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
