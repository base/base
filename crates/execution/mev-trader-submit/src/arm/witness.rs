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

use super::custody::{CustodyError, HotWalletKey, ProductionCustodyFailure};
use super::proofs::{
    CodeHashProvider, DeploymentEvidence, G7Attestation, LiveRunAttestation,
    ProofVerificationError, SubmitSuppressionClear,
};
use super::request::{self, RequestSpec};
use super::suppression::{SuppressionEpochStore, SuppressionFileStore, SuppressionRollbackError};
use super::{ArmError, ArmedFailSink};
use crate::PriorityEconomicsReceipt;
use crate::signer::SignerError;

/// Exact proof or authority failure that closes a production candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductionCandidateError {
    /// G7 proof verification failed.
    G7(ProofVerificationError),
    /// Live-window proof verification failed.
    Live(ProofVerificationError),
    /// Deployment proof verification failed.
    Deployment(ProofVerificationError),
    /// Authenticated proof campaigns differed.
    CampaignMismatch,
    /// Suppression state could not be read.
    SuppressionUnavailable,
    /// Submission was actively suppressed.
    Suppressed,
    /// Suppression state rolled back.
    SuppressionRollback,
    /// Suppression state was malformed.
    SuppressionInvalid,
    /// The process kill state was active or unavailable.
    KillActive,
    /// Settled loss was unavailable or incomplete.
    SettledLoss(super::SettledLossUnavailableReason),
    /// Committed state could not be read.
    CommittedStateUnavailable,
    /// The committed account was absent.
    CommittedAccountAbsent,
    /// The complete authorization gate refused the candidate.
    Gate(AuthorizationGateError),
}

/// Exact authorization conjunction failure after checked proof import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationGateError {
    /// Candidate principal differed from the submit context.
    AmountMismatch,
    /// The master submit gate closed.
    SubmitGate(base_mev_trader::ClosedReason),
    /// Claim victim differed from the candidate.
    ClaimVictimMismatch,
    /// Claim chain differed from Base.
    ClaimChainMismatch,
    /// Claim campaign differed from the candidate.
    ClaimCampaignMismatch,
    /// Claim-store identity differed from deployment evidence.
    ClaimStoreIdentityMismatch,
    /// Deployment executor differed from the candidate.
    ExecutorMismatch,
    /// Live proof did not cover the candidate campaign.
    LiveCoverageMismatch,
    /// G7 proof did not cover the candidate campaign.
    G7CoverageMismatch,
}

/// Base chain id (compile-pinned).
pub const CHAIN_ID_BASE: u64 = 8453;

/// Bounded T4e shape evidence retained for durable simulation projection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SimulationIdentityEvidence {
    pub(crate) parent_hash: B256,
    pub(crate) block_number: u64,
    pub(crate) sender: Address,
    pub(crate) nonce: u64,
    pub(crate) chain_id: u64,
    pub(crate) gas_limit: u64,
    pub(crate) max_fee_per_gas: u128,
    pub(crate) max_priority_fee_per_gas: u128,
    pub(crate) valid_until_block: u64,
    pub(crate) hop_protocols: [u8; 2],
    pub(crate) hop_adapters: [Address; 2],
    pub(crate) hop_runtime_hashes: [B256; 2],
}

/// The identity a candidate is bound to, derived from the validated tx + campaign.
/// Private: constructed only by [`CheckedCandidate::new`].
#[derive(Debug, Clone, Copy)]
pub struct ValidatedExecutionIdentity {
    campaign_id: CampaignId,
    victim: B256,
    plan_digest: B256,
    amount: U256,
    executor: Address,
    economics: Option<PriorityEconomicsReceipt>,
    simulation: Option<SimulationIdentityEvidence>,
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

    /// Checked economics retained from the sole positive-EV evaluator.
    pub const fn economics(&self) -> Option<PriorityEconomicsReceipt> {
        self.economics
    }

    /// Bounded T4e transaction and route evidence.
    pub(crate) const fn simulation_evidence(&self) -> Option<SimulationIdentityEvidence> {
        self.simulation
    }
    #[cfg(test)]
    pub(crate) fn for_simulation_store_test(
        campaign_id: CampaignId,
        victim: B256,
        plan_digest: B256,
        economics: PriorityEconomicsReceipt,
    ) -> Self {
        Self {
            campaign_id,
            victim,
            plan_digest,
            amount: U256::from_limbs([1, 0, 0, 0]),
            executor: Address::ZERO,
            economics: Some(economics),
            simulation: Some(SimulationIdentityEvidence {
                parent_hash: B256::repeat_byte(7),
                block_number: economics.authority_block(),
                sender: Address::repeat_byte(8),
                nonce: 9,
                chain_id: CHAIN_ID_BASE,
                gas_limit: 100_000,
                max_fee_per_gas: 300,
                max_priority_fee_per_gas: 100,
                valid_until_block: economics.authority_block() + 1,
                hop_protocols: [0, 3],
                hop_adapters: [Address::repeat_byte(10), Address::repeat_byte(11)],
                hop_runtime_hashes: [B256::repeat_byte(12), B256::repeat_byte(13)],
            }),
        }
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

impl ProofBindings {
    pub(crate) const fn deployment_code_hash(&self) -> B256 {
        self.deployment_code_hash
    }

    pub(crate) const fn deployment_digest(&self) -> B256 {
        self.deployment_digest
    }

    pub(crate) const fn binary_digest(&self) -> B256 {
        self.binary_digest
    }

    pub(crate) const fn r9_store_identity(&self) -> StoreIdentity {
        self.r9_store_identity
    }

    pub(crate) const fn valid_until_block(&self) -> u64 {
        self.valid_until_block
    }
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

    fn economics(&self) -> Option<PriorityEconomicsReceipt> {
        match self {
            Self::Legacy(_) => None,
            #[cfg(feature = "t4e-handoff")]
            Self::Authority { tx, access: _ } => Some(tx.economics()),
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
            economics: None,
            simulation: None,
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
        let observation = vtx.observation();
        let frame = observation.frame();
        let simulation = Some(SimulationIdentityEvidence {
            parent_hash: frame.parent_hash,
            block_number: frame.block_number,
            sender: observation.sender(),
            nonce: observation.nonce(),
            chain_id: observation.chain_id(),
            gas_limit: observation.gas_limit(),
            max_fee_per_gas: observation.max_fee_per_gas(),
            max_priority_fee_per_gas: observation.max_priority_fee_per_gas(),
            valid_until_block: observation.valid_until_block(),
            hop_protocols: observation.hop_protocols().map(|protocol| protocol as u8),
            hop_adapters: observation.hop_adapters(),
            hop_runtime_hashes: observation.hop_runtime_hashes(),
        });
        let source = CheckedTx::Authority { tx: vtx, access };
        let id = ValidatedExecutionIdentity {
            campaign_id,
            victim: source.victim(),
            plan_digest: source.plan_digest(),
            amount: source.amount(),
            executor: source.executor(),
            economics: source.economics(),
            simulation,
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

/// Stable signed-field identity for a production signature mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionSignedField {
    /// Chain ID.
    ChainId,
    /// Nonce.
    Nonce,
    /// Gas limit.
    Gas,
    /// Maximum fee per gas.
    MaxFeePerGas,
    /// Maximum priority fee per gas.
    MaxPriorityFeePerGas,
    /// Destination.
    To,
    /// Value.
    Value,
    /// Calldata.
    Data,
}

/// Stable bounded production signing failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionSignFailure {
    /// Cryptographic signing failed.
    Sign,
    /// The envelope was not EIP-1559.
    NotEip1559,
    /// The signature was degenerate.
    DegenerateSignature,
    /// The fixed dummy signature was observed.
    DummySignature,
    /// The signature used high-s.
    HighS,
    /// The access list was not empty.
    NonEmptyAccessList,
    /// A signed field differed from the authorized transaction.
    FieldMismatch(ProductionSignedField),
    /// Signature recovery failed.
    Unrecoverable,
    /// The signer identity differed.
    SignerMismatch,
}

/// Exact phase and latch result for production custody/signing failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionSigningError {
    /// Existing process poison prevented custody access.
    PrecheckPoisoned,
    /// Loading the pinned key failed.
    Custody {
        /// Stable custody source.
        reason: ProductionCustodyFailure,
        /// Mandatory fail-stop latch outcome.
        latch: super::ProductionLatchOutcome,
    },
    /// Signing or signed-envelope verification failed.
    Sign {
        /// Stable signing source.
        reason: ProductionSignFailure,
        /// Mandatory fail-stop latch outcome.
        latch: super::ProductionLatchOutcome,
    },
}

impl From<SignerError> for ProductionSignFailure {
    fn from(error: SignerError) -> Self {
        match error {
            SignerError::Sign => Self::Sign,
            SignerError::NotEip1559 => Self::NotEip1559,
            SignerError::DegenerateSignature => Self::DegenerateSignature,
            SignerError::DummySignature => Self::DummySignature,
            SignerError::HighS => Self::HighS,
            SignerError::NonEmptyAccessList => Self::NonEmptyAccessList,
            SignerError::FieldMismatch(field) => Self::FieldMismatch(match field {
                "chainId" => ProductionSignedField::ChainId,
                "nonce" => ProductionSignedField::Nonce,
                "gas" => ProductionSignedField::Gas,
                "maxFeePerGas" => ProductionSignedField::MaxFeePerGas,
                "maxPriorityFeePerGas" => ProductionSignedField::MaxPriorityFeePerGas,
                "to" => ProductionSignedField::To,
                "value" => ProductionSignedField::Value,
                "data" => ProductionSignedField::Data,
                _ => unreachable!("signer emitted an unknown field identity"),
            }),
            SignerError::Unrecoverable => Self::Unrecoverable,
            SignerError::SignerMismatch => Self::SignerMismatch,
        }
    }
}

/// A candidate authorized by all five proofs and the R9 claim, carrying the proof
/// bindings for egress-moment re-validation.
#[derive(Debug)]
pub struct AuthorizedCandidate {
    cand: CheckedCandidate,
    bindings: ProofBindings,
}

impl AuthorizedCandidate {
    /// Issues the sole production witness with exact gate and binding failures.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_detailed(
        ctx: SubmitContext<'_>,
        sup: SubmitSuppressionClear,
        g7: G7Attestation,
        claim: VictimClaim,
        live: LiveRunAttestation,
        deploy: DeploymentEvidence,
        cand: CheckedCandidate,
    ) -> Result<Self, ProductionCandidateError> {
        if ctx.amount_in_wei != cand.id.amount {
            return Err(ProductionCandidateError::Gate(AuthorizationGateError::AmountMismatch));
        }
        let gate = match submit_gate(ctx) {
            SubmitDecision::Open => Ok(()),
            SubmitDecision::Closed(reason) => {
                Err(ProductionCandidateError::Gate(AuthorizationGateError::SubmitGate(reason)))
            }
        };
        Self::issue_detailed_inner(gate, sup, g7, claim, live, deploy, cand)
    }

    /// Compatibility wrapper that deliberately collapses detailed production failures.
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
        Self::issue_detailed(ctx, sup, g7, claim, live, deploy, cand).ok()
    }

    #[allow(clippy::too_many_arguments)]
    fn issue_detailed_inner(
        gate: Result<(), ProductionCandidateError>,
        sup: SubmitSuppressionClear,
        g7: G7Attestation,
        claim: VictimClaim,
        live: LiveRunAttestation,
        deploy: DeploymentEvidence,
        cand: CheckedCandidate,
    ) -> Result<Self, ProductionCandidateError> {
        gate?;
        if claim.victim_tx_hash() != cand.id.victim {
            return Err(ProductionCandidateError::Gate(
                AuthorizationGateError::ClaimVictimMismatch,
            ));
        }
        if claim.chain_id() != CHAIN_ID_BASE {
            return Err(ProductionCandidateError::Gate(AuthorizationGateError::ClaimChainMismatch));
        }
        if claim.campaign_id() != cand.id.campaign_id {
            return Err(ProductionCandidateError::Gate(
                AuthorizationGateError::ClaimCampaignMismatch,
            ));
        }
        if claim.store_identity() != deploy.r9_store_identity() {
            return Err(ProductionCandidateError::Gate(
                AuthorizationGateError::ClaimStoreIdentityMismatch,
            ));
        }
        if deploy.executor() != cand.id.executor {
            return Err(ProductionCandidateError::Gate(AuthorizationGateError::ExecutorMismatch));
        }
        if !live.covers(cand.id.campaign_id) {
            return Err(ProductionCandidateError::Gate(
                AuthorizationGateError::LiveCoverageMismatch,
            ));
        }
        if !g7.covers(cand.id.campaign_id) {
            return Err(ProductionCandidateError::Gate(AuthorizationGateError::G7CoverageMismatch));
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
        Ok(Self { cand, bindings })
    }

    /// Test-only seam for the proof-binding conjunction without the production owner pin.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn issue_with_gate_for_test(
        gate_open: bool,
        sup: SubmitSuppressionClear,
        g7: G7Attestation,
        claim: VictimClaim,
        live: LiveRunAttestation,
        deploy: DeploymentEvidence,
        cand: CheckedCandidate,
    ) -> Option<Self> {
        let gate = if gate_open {
            Ok(())
        } else {
            Err(ProductionCandidateError::Gate(AuthorizationGateError::SubmitGate(
                base_mev_trader::ClosedReason::NotArmed,
            )))
        };
        Self::issue_detailed_inner(gate, sup, g7, claim, live, deploy, cand).ok()
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
        self.load_and_sign_detailed(sink).map_err(|error| match error {
            ProductionSigningError::PrecheckPoisoned => ArmError::Poisoned,
            ProductionSigningError::Custody { latch, .. }
            | ProductionSigningError::Sign { latch, .. } => match latch {
                super::ProductionLatchOutcome::Engaged => {
                    ArmError::KillReason(KillReason::KeyOrSignatureFailure)
                }
                super::ProductionLatchOutcome::PersistFailed => ArmError::LatchPersistFailed,
                super::ProductionLatchOutcome::AlreadyPoisoned => ArmError::Poisoned,
            },
        })
    }

    /// Loads and signs through the pinned production custody path with exact phase failure.
    pub fn load_and_sign_detailed(
        self,
        sink: &Arc<ArmedFailSink>,
    ) -> Result<AuthorizedSignedSubmission, ProductionSigningError> {
        if sink.check().is_err() {
            return Err(ProductionSigningError::PrecheckPoisoned);
        }
        let key = HotWalletKey::load().map_err(|error| ProductionSigningError::Custody {
            reason: ProductionCustodyFailure::from(error),
            latch: sink.latch_production(),
        })?;
        let signed = key.sign_unsigned(self.cand.vtx.unsigned_tx()).map_err(|error| {
            ProductionSigningError::Sign {
                reason: ProductionSignFailure::from(error),
                latch: sink.latch_production(),
            }
        })?;
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
    /// Test-only: OR-inject an open submit gate without invoking the production owner
    /// pin. Never present in a non-test build; can only widen the gate to `Open` in
    /// tests, never close a real gate.
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
            AuthorizedCandidate::issue_with_gate_for_test(true, sup, g7, claim, live, deploy, cand)
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
            AuthorizedCandidate::issue_with_gate_for_test(
                false, sup, g7, claim, live, deploy, cand
            )
            .is_none()
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
            AuthorizedCandidate::issue_with_gate_for_test(true, sup, g7, claim, live, deploy, cand)
                .is_none()
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
            AuthorizedCandidate::issue_with_gate_for_test(true, sup, g7, claim, live, deploy, cand)
                .is_none()
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
            AuthorizedCandidate::issue_with_gate_for_test(true, sup, g7, claim, live, deploy, cand)
                .is_none()
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
            AuthorizedCandidate::issue_with_gate_for_test(true, sup, g7, claim, live, deploy, cand)
                .is_none()
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
