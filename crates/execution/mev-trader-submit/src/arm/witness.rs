//! Linear-typestate witness. The only path from a validated unsigned tx to a real
//! egress runs through this chain, and every step consumes the previous value:
//!
//! ```text
//! ValidatedUnsignedAtomicTx        (assembler-only; tx bytes bound to plan)
//!   -> CheckedCandidate            (id derived from the validated tx + campaign)
//!   -> AuthorizedCandidate::issue  (consumes ALL 5 proofs + R9 claim; captures bindings)
//!   -> load_and_sign               (internal custody signer; no external &SigningKey)
//!   -> AuthorizedSignedSubmission
//!   -> PairedSubmission::assemble  (inclusion + attribution request specs)
//!   -> transport::send_gated       (FULL fresh re-validation at the egress moment)
//! ```

use std::sync::Arc;

#[cfg(test)]
use alloy_consensus::SignableTransaction;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_mev_trader::{
    ArmedCriteria, CampaignId, DrawdownInput, KillReason, StartupError, StoreIdentity,
    SubmitContext, SubmitDecision, VictimClaim, open_anchored_killstate, submit_gate,
};

use crate::assembler::ValidatedUnsignedAtomicTx as LegacyValidatedUnsignedAtomicTx;
#[cfg(feature = "t4e-handoff")]
use crate::tx_authority::{BridgeConversionSeal, ValidatedUnsignedAtomicTx};

use super::custody::{CustodyError, HotWalletKey};
use super::proofs::{
    CodeHashProvider, DeploymentEvidence, G7Attestation, LiveRunAttestation, SubmitSuppressionClear,
};
use super::request::{self, RequestSpec};
use super::suppression::{SuppressionEpochStore, SuppressionFileStore, SuppressionRollbackError};
use super::{ArmError, ArmedFailSink};

/// Base chain id (compile-pinned).
pub const CHAIN_ID_BASE: u64 = 8453;

/// The identity a candidate is bound to, derived from the validated tx + campaign.
/// Private: constructed only by [`CheckedCandidate::new`].
#[derive(Debug, Clone, Copy)]
pub struct ValidatedExecutionIdentity {
    campaign_id: CampaignId,
    victim: B256,
    plan_digest: B256,
    amount: U256,
    executor: Address,
}

impl ValidatedExecutionIdentity {
    /// The campaign this identity is scoped to (accessed as a field in `issue`).
    pub const fn campaign_id(&self) -> CampaignId {
        self.campaign_id
    }
    /// The bound victim transaction hash.
    pub const fn victim(&self) -> B256 {
        self.victim
    }
    /// The plan principal.
    pub const fn amount(&self) -> U256 {
        self.amount
    }
    /// The executor address.
    pub const fn executor(&self) -> Address {
        self.executor
    }
    /// The plan digest (provenance; captured with the identity per §3.1).
    pub const fn plan_digest(&self) -> B256 {
        self.plan_digest
    }
}

/// The proof-derived bindings captured at issue time and re-checked for equality
/// at the egress moment. Private: constructed only by [`AuthorizedCandidate::issue`].
#[derive(Debug, Clone, Copy)]
pub struct ProofBindings {
    g7_expiry: u64,
    live_window_start: u64,
    live_expiry: u64,
    suppression_epoch: u64,
    deployment_code_hash: B256,
    deployment_digest: B256,
    binary_digest: B256,
    r9_store_identity: StoreIdentity,
    valid_until_block: u64,
}

#[derive(Debug)]
enum CheckedTx {
    Legacy(LegacyValidatedUnsignedAtomicTx),
    #[cfg(feature = "t4e-handoff")]
    Authority {
        tx: ValidatedUnsignedAtomicTx,
        access: BridgeConversionSeal,
    },
}

impl CheckedTx {
    fn victim(&self) -> B256 {
        match self {
            Self::Legacy(tx) => tx.victim(),
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access: _ } => tx.observation().victim(),
        }
    }

    fn plan_digest(&self) -> B256 {
        match self {
            Self::Legacy(tx) => tx.plan_digest(),
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access: _ } => tx.observation().plan_digest(),
        }
    }

    fn amount(&self) -> U256 {
        match self {
            Self::Legacy(tx) => tx.amount(),
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access: _ } => tx.amount(),
        }
    }

    fn executor(&self) -> Address {
        match self {
            Self::Legacy(tx) => tx.executor(),
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access: _ } => tx.observation().executor(),
        }
    }

    fn valid_until_block(&self) -> u64 {
        match self {
            Self::Legacy(tx) => tx.valid_until_block(),
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access: _ } => tx.observation().valid_until_block(),
        }
    }

    fn unsigned_tx(&self) -> &alloy_consensus::TxEip1559 {
        match self {
            Self::Legacy(tx) => tx.unsigned_tx(),
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access } => tx.unsigned_tx_with_bridge_access(access),
        }
    }
}

/// A validated candidate whose id is derived from the assembler-only tx witness.
#[derive(Debug)]
pub struct CheckedCandidate {
    vtx: CheckedTx,
    id: ValidatedExecutionIdentity,
}

impl CheckedCandidate {
    /// Derives the execution identity from the assembler-validated tx and the
    /// campaign being run. Because `vtx` can only be produced by the assembler,
    /// the id is guaranteed to describe those exact tx bytes.
    pub const fn new(vtx: LegacyValidatedUnsignedAtomicTx, campaign_id: CampaignId) -> Self {
        let id = ValidatedExecutionIdentity {
            campaign_id,
            victim: vtx.victim(),
            plan_digest: vtx.plan_digest(),
            amount: vtx.amount(),
            executor: vtx.executor(),
        };
        Self { vtx: CheckedTx::Legacy(vtx), id }
    }

    /// Joins a freshly revalidated T4d authority witness through an unforgeable bridge seal.
    #[cfg(feature = "t4e-handoff")]
    pub fn from_authority(
        vtx: ValidatedUnsignedAtomicTx,
        campaign_id: CampaignId,
        access: BridgeConversionSeal,
    ) -> Self {
        let source = CheckedTx::Authority { tx: vtx, access };
        let id = ValidatedExecutionIdentity {
            campaign_id,
            victim: source.victim(),
            plan_digest: source.plan_digest(),
            amount: source.amount(),
            executor: source.executor(),
        };
        Self { vtx: source, id }
    }

    #[cfg(test)]
    pub(crate) const fn identity(&self) -> ValidatedExecutionIdentity {
        self.id
    }

    #[cfg(test)]
    pub(crate) fn valid_until_block(&self) -> u64 {
        self.vtx.valid_until_block()
    }

    #[cfg(test)]
    pub(crate) fn unsigned_signing_hash(&self) -> B256 {
        self.vtx.unsigned_tx().signature_hash()
    }
}

/// A candidate authorized by ALL five proofs + the R9 claim, carrying the proof
/// bindings for the egress-moment re-validation.
#[derive(Debug)]
pub struct AuthorizedCandidate {
    cand: CheckedCandidate,
    bindings: ProofBindings,
}

impl AuthorizedCandidate {
    /// Issue authorization iff EVERY predicate holds. Consumes `ctx` (submit gate),
    /// the suppression clear, the G7 + live-run attestations, the R9 claim, and the
    /// deployment evidence, binding them to the candidate. Any mismatch is `None`.
    // Production entrypoint (forward B5 API); tests drive `issue_checked` (a
    // dependent crate cannot self-forge an armed `ArmedCriteria` — the armed
    // constructor is `#[cfg(test)]`-only — so B5 injects a verified value from
    // `base_mev_trader::production_arming_criteria`, which this crate never calls).
    // `too_many_arguments`: the proof-conjunction deliberately consumes
    // all five proofs + claim + candidate by value (linear ownership).
    #[allow(clippy::too_many_arguments)]
    pub fn issue(
        ctx: SubmitContext<'_>,
        sup: SubmitSuppressionClear,
        g7: G7Attestation,
        claim: VictimClaim,
        live: LiveRunAttestation,
        deploy: DeploymentEvidence,
        cand: CheckedCandidate,
    ) -> Option<Self> {
        // amount first (no clone_view of ctx before submit_gate consumes it): on an
        // amount mismatch the gate is not even evaluated.
        let gate_open = if ctx.amount_in_wei != cand.id.amount {
            false
        } else {
            matches!(submit_gate(ctx), SubmitDecision::Open)
        };
        Self::issue_inner(gate_open, sup, g7, claim, live, deploy, cand)
    }

    /// PRIVATE proof-binding conjunction with the submit-gate decision reduced to a
    /// bool. It is a module-private `fn` — NOT `pub` — so no code outside
    /// `witness.rs` (including post-G4 arm wiring) can invoke it to bypass the real
    /// `submit_gate`. Production `issue` derives `gate_open` from the real gate; the
    /// test-only `issue_checked` seam wraps it (a dependent crate cannot ARM
    /// `ArmedCriteria`, so the real gate can never open in-crate tests).
    #[allow(clippy::too_many_arguments)]
    fn issue_inner(
        gate_open: bool,
        sup: SubmitSuppressionClear,
        g7: G7Attestation,
        claim: VictimClaim,
        live: LiveRunAttestation,
        deploy: DeploymentEvidence,
        cand: CheckedCandidate,
    ) -> Option<Self> {
        if !gate_open {
            return None;
        }
        if claim.victim_tx_hash() != cand.id.victim
            || claim.chain_id() != CHAIN_ID_BASE
            || claim.campaign_id() != cand.id.campaign_id
        {
            return None;
        }
        if claim.store_identity() != deploy.r9_store_identity() {
            return None;
        }
        if deploy.executor() != cand.id.executor {
            return None;
        }
        if !live.covers(cand.id.campaign_id) || !g7.covers(cand.id.campaign_id) {
            return None;
        }
        let bindings = ProofBindings {
            g7_expiry: g7.expiry(),
            live_window_start: live.window_start(),
            live_expiry: live.expiry(),
            suppression_epoch: sup.epoch(),
            deployment_code_hash: deploy.code_hash(),
            deployment_digest: deploy.deployment_digest(),
            binary_digest: deploy.binary_digest(),
            r9_store_identity: deploy.r9_store_identity(),
            valid_until_block: cand.vtx.valid_until_block(),
        };
        Some(Self { cand, bindings })
    }

    /// Test-only seam: exercise the proof-binding conjunction with the submit-gate
    /// decision injected (a dependent crate cannot ARM `ArmedCriteria`). `#[cfg(test)]`
    /// so it can NEVER be reached by production/arm-wiring code.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue_checked(
        gate_open: bool,
        sup: SubmitSuppressionClear,
        g7: G7Attestation,
        claim: VictimClaim,
        live: LiveRunAttestation,
        deploy: DeploymentEvidence,
        cand: CheckedCandidate,
    ) -> Option<Self> {
        Self::issue_inner(gate_open, sup, g7, claim, live, deploy, cand)
    }

    /// Load the funded hot wallet from custody and sign the validated tx. On any
    /// key-load or signing failure it latches the durable fail-stop and returns the
    /// mapped [`ArmError`]. There is NO external `&SigningKey`: the key is loaded,
    /// used, and dropped (zeroized) inside custody.
    // Production entrypoint; tests drive `load_and_sign_with` (they cannot present
    // the pinned funded key).
    pub fn load_and_sign(
        self,
        sink: &Arc<ArmedFailSink>,
    ) -> Result<AuthorizedSignedSubmission, ArmError> {
        self.sign_inner(sink, HotWalletKey::load)
    }

    /// Test-only seam: inject a temp-file loader so the success path is exercisable
    /// without the pinned funded key. `#[cfg(test)]` so production/arm-wiring code
    /// can NEVER load a key from an arbitrary path/address.
    #[cfg(test)]
    pub(crate) fn load_and_sign_with<F>(
        self,
        sink: &Arc<ArmedFailSink>,
        loader: F,
    ) -> Result<AuthorizedSignedSubmission, ArmError>
    where
        F: FnOnce() -> Result<HotWalletKey, CustodyError>,
    {
        self.sign_inner(sink, loader)
    }

    /// PRIVATE signer core (module-private `fn`, not `pub`): loads via
    /// `loader`, signs, and fail-stops on error. Production passes the pinned
    /// [`HotWalletKey::load`]; only the test seam can pass another loader.
    fn sign_inner<F>(
        self,
        sink: &Arc<ArmedFailSink>,
        loader: F,
    ) -> Result<AuthorizedSignedSubmission, ArmError>
    where
        F: FnOnce() -> Result<HotWalletKey, CustodyError>,
    {
        sink.check()?;
        let key = loader().map_err(|_| sink.latch(KillReason::KeyOrSignatureFailure))?;
        let signed = key
            .sign_unsigned(self.cand.vtx.unsigned_tx())
            .map_err(|_| sink.latch(KillReason::KeyOrSignatureFailure))?;
        let raw_tx = Bytes::from(signed.raw);
        let raw_tx_hash = keccak256(raw_tx.as_ref());
        Ok(AuthorizedSignedSubmission {
            cand: self.cand,
            bindings: self.bindings,
            raw_tx,
            raw_tx_hash,
            signer: key.address(),
        })
    }
}

/// A signed, authorized submission carrying the raw backrun bytes + bindings.
#[derive(Debug)]
pub struct AuthorizedSignedSubmission {
    cand: CheckedCandidate,
    bindings: ProofBindings,
    raw_tx: Bytes,
    raw_tx_hash: B256,
    signer: Address,
}

impl AuthorizedSignedSubmission {
    /// The signed raw backrun envelope bytes (inclusion channel payload).
    pub const fn raw_tx(&self) -> &Bytes {
        &self.raw_tx
    }
    /// `keccak256(raw_tx)` — the expected inclusion hash.
    pub const fn raw_tx_hash(&self) -> B256 {
        self.raw_tx_hash
    }
    /// The recovered signer address (the funded wallet).
    pub const fn signer(&self) -> Address {
        self.signer
    }
    /// The bound victim transaction hash (attribution channel slot 0).
    pub const fn victim(&self) -> B256 {
        self.cand.id.victim()
    }
    /// The execution identity.
    pub const fn id(&self) -> ValidatedExecutionIdentity {
        self.cand.id
    }
    /// The captured proof bindings.
    pub const fn bindings(&self) -> ProofBindings {
        self.bindings
    }
}

/// The two assembled channel request specs + bindings/id/expected inclusion hash.
#[derive(Debug)]
pub struct PairedSubmission {
    pub(crate) inclusion: RequestSpec,
    pub(crate) attribution: RequestSpec,
    pub(crate) bindings: ProofBindings,
    pub(crate) id: ValidatedExecutionIdentity,
    pub(crate) expected_inclusion_hash: B256,
}

impl PairedSubmission {
    /// Assemble the inclusion + attribution request specs from a signed submission
    /// (consumes it; pure — no network).
    pub fn assemble(subm: AuthorizedSignedSubmission) -> Self {
        let inclusion = request::build_inclusion(&subm);
        let attribution = request::build_attribution(&subm);
        Self {
            inclusion,
            attribution,
            bindings: subm.bindings,
            id: subm.id(),
            expected_inclusion_hash: subm.raw_tx_hash(),
        }
    }
}

// -- freshness sources --------------------------------------------------------

/// A monotonic clock source (`now_unix`). Real impl reads the system clock; tests
/// inject a fixed value.
pub(crate) trait TimeSource {
    /// Current unix time in seconds, or `None` on an unavailable/invalid clock.
    fn now_unix(&self) -> Option<u64>;
}

/// The dynamic realized-loss accounting source (`SubmitContext` carries the value;
/// this is re-read fresh at the egress moment).
pub trait DrawdownSource {
    /// The current authoritative drawdown accounting.
    fn load(&self) -> DrawdownInput;
}

/// The current release/store identity triple, re-checked for equality at egress
/// (a changed binary/deployment/store identity blocks egress).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentIdentity {
    /// The running binary digest.
    pub binary_digest: B256,
    /// The active deployment digest.
    pub deployment_digest: B256,
    /// The active R9 store identity.
    pub r9_store_identity: StoreIdentity,
}

/// Source of the current [`DeploymentIdentity`].
pub trait DeploymentIdentitySource {
    /// The current identity triple, or `None` if unavailable (fail-closed).
    fn current(&self) -> Option<DeploymentIdentity>;
}

/// Linear proof that egress-moment receipt freshness and kill-clear checks both held.
#[derive(Debug)]
pub struct FreshnessProof {
    signed_receipt: SignedReceiptFresh,
    kill: KillClear,
}

#[derive(Debug)]
struct SignedReceiptFresh {
    private: (),
}

#[derive(Debug)]
struct KillClear {
    private: (),
}

impl FreshnessProof {
    /// Whether this proof was minted after the signed live-run window check.
    pub const fn signed_receipt_fresh(&self) -> bool {
        matches!(self.signed_receipt, SignedReceiptFresh { private: () })
    }

    /// Whether this proof was minted after the authoritative kill-clear check.
    pub const fn kill_clear(&self) -> bool {
        matches!(self.kill, KillClear { private: () })
    }
}

/// All authoritative freshness backings, re-read at the egress moment. Owns the
/// shared [`ArmedFailSink`] so `send_gated` is a two-argument call.
pub struct FreshnessSources<'a> {
    pub(crate) armed: &'a ArmedCriteria,
    pub(crate) drawdown: &'a dyn DrawdownSource,
    pub(crate) suppression_file: &'a SuppressionFileStore,
    pub(crate) suppression_epoch: &'a SuppressionEpochStore,
    pub(crate) code_hash: &'a dyn CodeHashProvider,
    pub(crate) deployment_identity: &'a dyn DeploymentIdentitySource,
    pub(crate) clock: &'a dyn TimeSource,
    pub(crate) sink: Arc<ArmedFailSink>,
    /// Test-only: OR-inject an open submit gate (a dependent crate cannot ARM
    /// `ArmedCriteria`). Never present in a non-test build; can only widen the gate
    /// to `Open` in tests, never close a real gate.
    #[cfg(test)]
    pub force_gate_open: bool,
}

impl core::fmt::Debug for FreshnessSources<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The backings are trait objects; expose nothing sensitive.
        formatter.debug_struct("FreshnessSources").finish_non_exhaustive()
    }
}

impl<'a> FreshnessSources<'a> {
    /// Test-only: assemble the freshness sources from ARBITRARY caller-supplied
    /// backings (temp stores, fixture providers, fake clock). `#[cfg(test)]` so
    /// production/arm-wiring code can NEVER inject fixture sources or arbitrary
    /// store paths — production builds them through [`ArmRuntime`], which owns the
    /// compile-pinned suppression stores and the system clock.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        armed: &'a ArmedCriteria,
        drawdown: &'a dyn DrawdownSource,
        suppression_file: &'a SuppressionFileStore,
        suppression_epoch: &'a SuppressionEpochStore,
        code_hash: &'a dyn CodeHashProvider,
        deployment_identity: &'a dyn DeploymentIdentitySource,
        clock: &'a dyn TimeSource,
        sink: Arc<ArmedFailSink>,
    ) -> Self {
        Self {
            armed,
            drawdown,
            suppression_file,
            suppression_epoch,
            code_hash,
            deployment_identity,
            clock,
            sink,
            force_gate_open: false,
        }
    }

    /// Test-only: force the submit gate open for the positive send path.
    #[cfg(test)]
    pub(crate) const fn with_forced_gate(mut self, open: bool) -> Self {
        self.force_gate_open = open;
        self
    }

    /// The full egress-moment re-validation conjunction (§3.3). Returns a
    /// non-forgeable proof only when every source is fresh and every binding holds.
    pub fn revalidate(
        &self,
        bindings: &ProofBindings,
        id: &ValidatedExecutionIdentity,
    ) -> Option<FreshnessProof> {
        // Kill observation is first and authoritative. In particular, the test-only forced-open
        // seam can never override a non-clear durable state or an already-poisoned process.
        let Ok(kill) = self.sink.observe_kill() else {
            return None;
        };
        let ctx = SubmitContext {
            armed: self.armed,
            amount_in_wei: id.amount(),
            drawdown: self.drawdown.load(),
            kill,
        };
        let gate_open = matches!(submit_gate(ctx), SubmitDecision::Open);
        #[cfg(test)]
        let gate_open = gate_open || self.force_gate_open;
        if !self.revalidate_gated(gate_open, bindings, id) {
            return None;
        }
        Some(FreshnessProof {
            signed_receipt: SignedReceiptFresh { private: () },
            kill: KillClear { private: () },
        })
    }

    /// The freshness conjunction with the gate decision reduced to a bool.
    fn revalidate_gated(
        &self,
        gate_open: bool,
        bindings: &ProofBindings,
        id: &ValidatedExecutionIdentity,
    ) -> bool {
        // (0) not poisoned.
        if self.sink.is_poisoned() {
            return false;
        }
        if !gate_open {
            return false;
        }
        // (2) suppression fresh: lock-guarded (fail-closed on a mid-write/stale
        // writer lock present before OR after the read), non-suppressed, epoch equal,
        // monotonic. Uses the SAME guarded read path as initial proof creation.
        let Some(record) = self.suppression_file.read_fresh_guarded() else {
            return false;
        };
        if record.suppressed || record.epoch != bindings.suppression_epoch {
            return false;
        }
        if self.suppression_epoch.observe(record.epoch).is_err() {
            return false;
        }
        // (3) attestation windows still open: G7 not expired, AND the live-run window
        // is currently OPEN (window_start <= now < expiry), re-checked at egress.
        let Some(now) = self.clock.now_unix() else {
            return false;
        };
        if now >= bindings.g7_expiry {
            return false;
        }
        if now < bindings.live_window_start || now >= bindings.live_expiry {
            return false;
        }
        // (4) on-chain code hash still equals the attested hash.
        let Ok(onchain) = self.code_hash.code_hash_at_latest_committed(id.executor()) else {
            return false;
        };
        if onchain != bindings.deployment_code_hash {
            return false;
        }
        // (5) release/store identity triple unchanged.
        let Some(current) = self.deployment_identity.current() else {
            return false;
        };
        if current.binary_digest != bindings.binary_digest
            || current.deployment_digest != bindings.deployment_digest
            || current.r9_store_identity != bindings.r9_store_identity
        {
            return false;
        }
        // (6) deadline still ahead.
        let Ok(block) = self.code_hash.current_block() else {
            return false;
        };
        block < bindings.valid_until_block
    }
}

/// Production time source: the system clock (unix seconds).
#[derive(Debug)]
pub(crate) struct SystemClock;

impl TimeSource for SystemClock {
    fn now_unix(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs())
    }
}

/// The production arm forward-B5 facade. It OWNS the compile-pinned suppression
/// file + high-water stores, the system clock, and the shared fail-stop sink,
/// building them INTERNALLY from the pinned paths. B5 injects ONLY the keyless
/// node-local providers (`CodeHashProvider`/`DrawdownSource`/
/// `DeploymentIdentitySource`) + the armed criteria; it can never point a store at
/// an arbitrary path nor inject a fixture source in production.
#[derive(Debug)]
pub struct ArmRuntime {
    suppression_file: SuppressionFileStore,
    suppression_epoch: SuppressionEpochStore,
    clock: SystemClock,
    sink: Arc<ArmedFailSink>,
}

/// Failure to open the production arm runtime.
#[derive(Debug)]
pub enum ArmRuntimeOpenError {
    /// The pinned anchor-backed kill-state owner or its initial clear observation failed.
    Startup(StartupError),
    /// The pinned suppression high-water store failed to open.
    Suppression(SuppressionRollbackError),
}

impl core::fmt::Display for ArmRuntimeOpenError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Startup(error) => write!(formatter, "arm runtime startup failed: {error}"),
            Self::Suppression(error) => {
                write!(formatter, "arm suppression startup failed: {error}")
            }
        }
    }
}

impl core::error::Error for ArmRuntimeOpenError {}

impl ArmRuntime {
    /// Open the runtime from the compile-pinned anchor and suppression stores.
    ///
    /// No production caller can inject a kill store, sink, or filesystem path.
    pub fn open() -> Result<Self, ArmRuntimeOpenError> {
        let kill = open_anchored_killstate().map_err(ArmRuntimeOpenError::Startup)?;
        let sink =
            Arc::new(ArmedFailSink::from_anchored(kill).map_err(ArmRuntimeOpenError::Startup)?);
        Ok(Self {
            suppression_file: SuppressionFileStore::at_pinned_path(),
            suppression_epoch: SuppressionEpochStore::open_pinned()
                .map_err(ArmRuntimeOpenError::Suppression)?,
            clock: SystemClock,
            sink,
        })
    }

    /// The shared fail-stop sink (for the claim façade / load-and-sign path).
    pub const fn sink(&self) -> &Arc<ArmedFailSink> {
        &self.sink
    }

    /// Build the current [`SubmitSuppressionClear`] proof from the pinned stores
    /// (fail-closed on lock/parse/rollback).
    pub fn suppression_clear(&self) -> Option<SubmitSuppressionClear> {
        SubmitSuppressionClear::read(&self.suppression_file, &self.suppression_epoch)
    }

    /// Assemble the egress-moment freshness sources: the pinned suppression stores +
    /// system clock come from `self`; the caller supplies ONLY the node-injected
    /// keyless providers and armed criteria.
    pub fn freshness<'a>(
        &'a self,
        armed: &'a ArmedCriteria,
        drawdown: &'a dyn DrawdownSource,
        code_hash: &'a dyn CodeHashProvider,
        deployment_identity: &'a dyn DeploymentIdentitySource,
    ) -> FreshnessSources<'a> {
        FreshnessSources {
            armed,
            drawdown,
            suppression_file: &self.suppression_file,
            suppression_epoch: &self.suppression_epoch,
            code_hash,
            deployment_identity,
            clock: &self.clock,
            sink: Arc::clone(&self.sink),
            #[cfg(test)]
            force_gate_open: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arm::custody::{CustodyError, HotWalletKey};
    use crate::arm::proofs::{ProviderError, SubmitSuppressionClear};
    use crate::arm::suppression::SuppressionFileStore;
    use crate::arm::testkit as tk;
    use alloy_primitives::B256;
    use base_mev_trader::CampaignId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn campaign() -> CampaignId {
        CampaignId::new([0x0Au8; 32])
    }

    struct PanicDrawdown(AtomicUsize);

    impl DrawdownSource for PanicDrawdown {
        fn load(&self) -> base_mev_trader::DrawdownInput {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("drawdown source reached before kill refusal")
        }
    }

    struct PanicCodeHash;

    impl CodeHashProvider for PanicCodeHash {
        fn code_hash_at_latest_committed(
            &self,
            _address: alloy_primitives::Address,
        ) -> Result<B256, ProviderError> {
            panic!("code-hash provider reached before kill refusal")
        }

        fn current_block(&self) -> Result<u64, ProviderError> {
            panic!("block provider reached before kill refusal")
        }

        fn native_balance_at_latest_committed(
            &self,
            _address: alloy_primitives::Address,
        ) -> Result<Option<alloy_primitives::U256>, ProviderError> {
            panic!("balance provider reached before kill refusal")
        }
    }

    struct PanicDeploymentIdentity;

    impl DeploymentIdentitySource for PanicDeploymentIdentity {
        fn current(&self) -> Option<DeploymentIdentity> {
            panic!("deployment identity reached before kill refusal")
        }
    }

    struct PanicClock;

    impl TimeSource for PanicClock {
        fn now_unix(&self) -> Option<u64> {
            panic!("clock reached before kill refusal")
        }
    }

    struct Setup {
        _dir: tk::TempDir,
        authorized: AuthorizedCandidate,
    }

    /// Builds a fully-consistent positive `AuthorizedCandidate` via the gate seam.
    fn positive() -> Setup {
        let now = 1_000;
        let dir = tk::TempDir::new("witness-pos");
        let code_hash = B256::repeat_byte(0x33);
        let (vtx, victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        let (claim, store) = tk::victim_claim(&dir.path, victim, campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            store,
        );
        let g7 = tk::g7(campaign(), now + 100, now);
        let live = tk::live(campaign(), now + 100, now);
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let file = SuppressionFileStore::new(&path);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup = SubmitSuppressionClear::read(&file, &epoch_store).expect("clear");
        let authorized =
            AuthorizedCandidate::issue_checked(true, sup, g7, claim, live, deploy, cand)
                .expect("issue");
        Setup { _dir: dir, authorized }
    }

    #[test]
    fn issue_positive_ok() {
        let _ = positive();
    }

    #[test]
    fn assemble_validated_rejects_non_base_chain() {
        use crate::assembler::{AssembleInput, HopExecutionParams, assemble_validated};
        use alloy_primitives::U256;
        use base_mev_trader::MeasurementContext;

        let (victim_raw, victim_hash) = tk::victim_env(37);
        let plan = tk::plan(victim_hash);
        let frame = MeasurementContext {
            parent_hash: plan.parent_hash,
            block_number: plan.block_number,
            predecessor_index: plan.predecessor_index,
            payload_id: plan.payload_id,
            victim: plan.victim,
        };
        let input = AssembleInput {
            plan: &plan,
            current_frame: frame,
            executor: tk::EXECUTOR,
            hops: [
                HopExecutionParams { adapter: tk::ADAPTER, min_amount_out: U256::from(1u64) },
                HopExecutionParams { adapter: tk::ADAPTER, min_amount_out: U256::from(1u64) },
            ],
            chain_id: 1, // Ethereum mainnet, NOT Base.
            nonce: 0,
            gas: 2_000_000,
            max_fee_per_gas: 1_000_000_000,
            valid_until_block: 12_345_678,
            victim_raw_tx: &victim_raw,
            victim_tx_hash: victim_hash,
            expected_victim_priority_fee: Some(37),
            priority_economics: None,
        };
        assert!(assemble_validated(&input).is_err(), "non-Base chain must be rejected");
    }

    #[test]
    fn issue_gate_closed_is_none() {
        let now = 1_000;
        let dir = tk::TempDir::new("witness-gate");
        let code_hash = B256::repeat_byte(0x33);
        let (vtx, victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        let (claim, store) = tk::victim_claim(&dir.path, victim, campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            store,
        );
        let g7 = tk::g7(campaign(), now + 100, now);
        let live = tk::live(campaign(), now + 100, now);
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup =
            SubmitSuppressionClear::read(&SuppressionFileStore::new(&path), &epoch_store).unwrap();
        // gate_open = false -> fail-closed.
        assert!(
            AuthorizedCandidate::issue_checked(false, sup, g7, claim, live, deploy, cand).is_none()
        );
    }

    #[test]
    fn issue_victim_mismatch_is_none() {
        let now = 1_000;
        let dir = tk::TempDir::new("witness-victim");
        let code_hash = B256::repeat_byte(0x33);
        let (vtx, _victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        // Claim a DIFFERENT victim than the candidate binds.
        let (claim, store) = tk::victim_claim(&dir.path, B256::repeat_byte(0xBB), campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            store,
        );
        let g7 = tk::g7(campaign(), now + 100, now);
        let live = tk::live(campaign(), now + 100, now);
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup =
            SubmitSuppressionClear::read(&SuppressionFileStore::new(&path), &epoch_store).unwrap();
        assert!(
            AuthorizedCandidate::issue_checked(true, sup, g7, claim, live, deploy, cand).is_none()
        );
    }

    #[test]
    fn issue_store_identity_mismatch_is_none() {
        let now = 1_000;
        let dir = tk::TempDir::new("witness-store");
        let code_hash = B256::repeat_byte(0x33);
        let (vtx, victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        let (claim, _store) = tk::victim_claim(&dir.path, victim, campaign());
        // Deployment bound to a DIFFERENT store identity than the claim's.
        let rogue = base_mev_trader::StoreIdentity::new([0xEE; 32]);
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            rogue,
        );
        let g7 = tk::g7(campaign(), now + 100, now);
        let live = tk::live(campaign(), now + 100, now);
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup =
            SubmitSuppressionClear::read(&SuppressionFileStore::new(&path), &epoch_store).unwrap();
        assert!(
            AuthorizedCandidate::issue_checked(true, sup, g7, claim, live, deploy, cand).is_none()
        );
    }

    #[test]
    fn issue_deploy_executor_mismatch_is_none() {
        let now = 1_000;
        let dir = tk::TempDir::new("witness-exec");
        let code_hash = B256::repeat_byte(0x33);
        let (vtx, victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        let (claim, store) = tk::victim_claim(&dir.path, victim, campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        // Deployment for a different executor address.
        let other_exec = alloy_primitives::Address::repeat_byte(0x77);
        let deploy = tk::deployment(
            &provider,
            other_exec,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            store,
        );
        let g7 = tk::g7(campaign(), now + 100, now);
        let live = tk::live(campaign(), now + 100, now);
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup =
            SubmitSuppressionClear::read(&SuppressionFileStore::new(&path), &epoch_store).unwrap();
        assert!(
            AuthorizedCandidate::issue_checked(true, sup, g7, claim, live, deploy, cand).is_none()
        );
    }

    #[test]
    fn issue_wrong_campaign_attestation_is_none() {
        let now = 1_000;
        let dir = tk::TempDir::new("witness-camp");
        let code_hash = B256::repeat_byte(0x33);
        let (vtx, victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        let (claim, store) = tk::victim_claim(&dir.path, victim, campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            store,
        );
        // g7/live for a DIFFERENT campaign -> covers() false.
        let other = CampaignId::new([0x0Bu8; 32]);
        let g7 = tk::g7(other, now + 100, now);
        let live = tk::live(other, now + 100, now);
        let path = tk::write_suppression_file(&dir.path, 5, false);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup =
            SubmitSuppressionClear::read(&SuppressionFileStore::new(&path), &epoch_store).unwrap();
        assert!(
            AuthorizedCandidate::issue_checked(true, sup, g7, claim, live, deploy, cand).is_none()
        );
    }

    #[test]
    fn load_and_sign_success_no_poison() {
        let setup = positive();
        let dir = tk::TempDir::new("witness-sign");
        let (key, address) = tk::hot_wallet_key();
        let wpath = tk::write_hot_wallet(&dir.path, &key);
        let sink = tk::sink(&dir.path);
        let signed = setup
            .authorized
            .load_and_sign_with(&sink, || HotWalletKey::load_from(&wpath, address))
            .expect("signed");
        assert_eq!(signed.signer(), address);
        assert!(!sink.is_poisoned());
    }

    #[test]
    fn sign_is_first_observer_after_clear_becomes_unknown() {
        let setup = positive();
        let (sink, store) = tk::mutable_sink();
        store.set(base_mev_trader::KillState::Unknown);

        let err = setup
            .authorized
            .load_and_sign_with(&sink, || panic!("signing loader reached before kill refusal"))
            .unwrap_err();

        assert!(matches!(err, ArmError::Poisoned));
        assert!(sink.is_poisoned());
    }

    #[test]
    fn egress_is_first_observer_after_clear_becomes_unknown() {
        let setup = positive();
        let dir = tk::TempDir::new("witness-egress-kill");
        let (key, address) = tk::hot_wallet_key();
        let wallet = tk::write_hot_wallet(&dir.path, &key);
        let (sink, store) = tk::mutable_sink();
        let signed = setup
            .authorized
            .load_and_sign_with(&sink, || HotWalletKey::load_from(&wallet, address))
            .expect("signed");
        let paired = PairedSubmission::assemble(signed);

        let suppression_path =
            tk::write_suppression_file(&dir.path, paired.bindings.suppression_epoch, false);
        let suppression_file = SuppressionFileStore::new(&suppression_path);
        let suppression_epoch = tk::epoch_store(&dir.path);
        let provider = PanicCodeHash;
        let deployment = PanicDeploymentIdentity;
        let drawdown = PanicDrawdown(AtomicUsize::new(0));
        let clock = PanicClock;
        let armed = tk::unarmed_criteria();
        let sources = FreshnessSources::new(
            &armed,
            &drawdown,
            &suppression_file,
            &suppression_epoch,
            &provider,
            &deployment,
            &clock,
            Arc::clone(&sink),
        )
        .with_forced_gate(true);

        store.set(base_mev_trader::KillState::Unknown);
        assert!(sources.revalidate(&paired.bindings, &paired.id).is_none());
        assert_eq!(drawdown.0.load(Ordering::SeqCst), 0);
        assert!(sink.is_poisoned());
    }

    #[test]
    fn load_and_sign_failure_latches_and_poisons() {
        let setup = positive();
        let dir = tk::TempDir::new("witness-fail");
        let sink = tk::sink(&dir.path);
        let err = setup.authorized.load_and_sign_with(&sink, || Err(CustodyError::Io)).unwrap_err();
        // Fail-stop: engaged the kill latch AND poisoned the process.
        assert!(matches!(err, ArmError::KillReason(_) | ArmError::LatchPersistFailed));
        assert!(sink.is_poisoned());
        assert!(matches!(sink.check(), Err(ArmError::Poisoned)));
    }
}
