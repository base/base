//! Rung-1: unsigned executor calldata + dummy-signature envelope + the two Blink
//! OFA channel structures. Byte-parity port of the TS
//! `scripts/arb-dryrun/blink-unsigned-assembler.ts`.
//!
//! Boundary: pure encoding with a fixed structurally-invalid dummy signature.
//! There is no signer, transport, nonce lookup, or submission path here. The
//! ephemeral real signer is the separate rung-2 [`crate::signer`] path; real
//! submission is the rung-3 boundary and is unavailable ([`submit_blocked`]).

use alloy_consensus::{SignableTransaction, TxEnvelope};
use alloy_eips::{
    eip2718::{Decodable2718, Encodable2718},
    eip2930::AccessList,
};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, b256, keccak256};
use alloy_sol_types::{SolCall, sol};
use base_mev_trader::{BackrunPlan, MeasurementContext, MeasurementEncoder};

use crate::fee::{FeeParityError, fee_bps_for_executor};

sol! {
    /// One executor swap hop — mirrors `BlinkAtomicExecutor.SwapHop` exactly.
    #[allow(missing_docs)]
    struct SwapHop {
        address adapter;
        address pool;
        address tokenIn;
        address tokenOut;
        uint24 feeBps;
        uint256 minAmountOut;
    }

    /// The two-hop atomic executor entrypoint.
    #[allow(missing_docs)]
    function executeBlinkOfaAtomic(
        SwapHop firstHop,
        SwapHop secondHop,
        uint256 amountIn,
        uint256 minFinalAmount,
        uint256 validUntilBlock
    );
}

/// The fixed high-entropy dummy signature `r` — FastLZ-incompressible, NOT
/// key-derived. Byte-matches the TS `MEASUREMENT_DUMMY_SIGNATURE.r`.
const DUMMY_R: B256 = b256!("5fdab2bc3e0846351de15a51b4f354bf4a4ce227302de002ac790bacef8ba802");
/// The fixed dummy signature `s` — an intentional EIP-2 HIGH-S value
/// (`s > secp256k1 n/2`) making the envelope non-broadcastable. Byte-matches the
/// TS `MEASUREMENT_DUMMY_SIGNATURE.s`.
const DUMMY_S: B256 = b256!("adccfdc48b0427d6d60ddfacca470a52f6924a603539118d356c152d1f0b5986");
/// The fixed dummy `yParity` (1). Byte-matches the TS dummy signature.
const DUMMY_Y_PARITY: bool = true;

/// The rung-1 envelope kind tag (mirrors the TS `kind`).
pub const UNSIGNED_ATOMIC_TX_KIND: &str = "blink-ofa-dummy-atomic-tx/v2";
/// The rung-1 dummy signature kind tag (mirrors the TS `signatureKind`).
pub const DUMMY_SIGNATURE_KIND: &str = "fixed-invalid-high-s-dummy";

/// The fixed dummy signature as an [`alloy_primitives::Signature`] (raw, NOT
/// normalized — the high-s value is preserved so the envelope stays invalid).
pub(crate) const fn dummy_signature() -> Signature {
    Signature::new(U256::from_be_bytes(DUMMY_R.0), U256::from_be_bytes(DUMMY_S.0), DUMMY_Y_PARITY)
}

/// Per-hop execution parameters NOT carried by the measurement [`BackrunPlan`].
///
/// R8 fee-SOURCE: the sizing fee is NO LONGER a caller input. It is carried in the
/// digest-bound [`BackrunPlan::route`] (`BackrunHop::fee_pips`) and converted to the
/// ABI `feeBps` through the single [`fee_bps_for_executor`] point. Only the adapter
/// address and the per-hop output floor — neither of which is priced — live here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopExecutionParams {
    /// The adapter contract that performs this hop's swap.
    pub adapter: Address,
    /// The strict per-hop output floor passed to the executor.
    pub min_amount_out: U256,
}

/// Inputs to [`assemble_unsigned_atomic_tx`].
#[derive(Debug, Clone, Copy)]
pub struct AssembleInput<'a> {
    /// The measurement plan whose route/amounts drive the calldata.
    pub plan: &'a BackrunPlan,
    /// The TRUSTED current-frame identity, sourced from the live
    /// `ProcessedFrame::measurement_context()` (NOT from the plan). The assembler
    /// compares ALL 5 fields `{parent_hash, block_number, predecessor_index,
    /// payload_id, victim}` against the plan EXACT before emit; any mismatch —
    /// including a same-parent but stale `predecessor_index`/`payload_id`
    /// generation, or a plan whose `victim` is not the current frame's victim — is
    /// fail-closed (no calldata). This is a SEPARATE gate from the digest self-check
    /// (which only catches field tampering, not staleness/wrong-frame). The
    /// raw-victim↔plan check in `assemble_unsigned_atomic_tx` is retained and
    /// complementary: it binds the raw envelope to the plan's victim, while this
    /// gate binds the plan's victim to the current frame.
    pub current_frame: MeasurementContext,
    /// The `BlinkAtomicExecutor` address (backrun `to`). Not carried by the
    /// plan — a deployment property supplied by the caller.
    pub executor: Address,
    /// Adapter + fee + min-out for `[firstHop, secondHop]`.
    pub hops: [HopExecutionParams; 2],
    /// Chain id (Base = 8453).
    pub chain_id: u64,
    /// Backrun nonce (a shape field; never looked up on-chain here).
    pub nonce: u64,
    /// Backrun gas limit.
    pub gas: u64,
    /// Backrun max fee per gas.
    pub max_fee_per_gas: u128,
    /// Executor `validUntilBlock` deadline.
    pub valid_until_block: u64,
    /// The signed victim EIP-1559 envelope, parsed locally only.
    pub victim_raw_tx: &'a [u8],
    /// The victim transaction hash (must equal `keccak256(victim_raw_tx)`).
    pub victim_tx_hash: B256,
    /// Optional feed priority fee used only to cross-check the raw envelope.
    pub expected_victim_priority_fee: Option<u128>,
}

/// The rung-1 output: the unsigned tx (for rung-2 to sign) plus its
/// non-broadcastable dummy serialization.
#[derive(Debug, Clone)]
pub struct UnsignedAtomicTx {
    /// Envelope kind tag.
    pub kind: &'static str,
    /// The unsigned EIP-1559 backrun transaction.
    pub unsigned_tx: alloy_consensus::TxEip1559,
    /// EIP-2718 bytes serialized with the fixed invalid dummy signature.
    pub dummy_signed_raw_tx: Vec<u8>,
    /// Always `true`: the dummy signature recovers no valid sender.
    pub non_broadcastable: bool,
    /// Dummy signature kind tag.
    pub signature_kind: &'static str,
    /// The bound victim transaction hash.
    pub target_tx_hash: B256,
    /// The victim priority fee copied onto the backrun (inclusion channel).
    pub victim_max_priority_fee_per_gas: u128,
}

/// A rung-1 assembly failure. The assembler is fail-closed: any error aborts
/// before an envelope is produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssembleError {
    /// A numeric shape field violated its positivity/bound constraint.
    InvalidField(&'static str),
    /// An address field was the zero address.
    ZeroAddress(&'static str),
    /// `min_final_amount` (plan `amount_out`) was below `amount_in`.
    MinFinalBelowPrincipal,
    /// The victim envelope was not a parseable signed EIP-1559 transaction.
    VictimNotEip1559,
    /// `keccak256(victim_raw_tx)` did not match the supplied victim hash.
    VictimHashMismatch,
    /// The victim hash did not match `plan.victim`.
    VictimNotBoundToPlan,
    /// The optional feed priority fee did not match the parsed envelope.
    VictimPriorityFeeMismatch,
    /// `max_fee_per_gas` was below the victim priority fee.
    MaxFeeBelowVictimPriority,
    /// The plan's self-excluding digest did not match its fields (field/fee tamper).
    DigestMismatch,
    /// The plan's frame identity did not match the trusted current frame (stale or
    /// wrong-parent/generation plan).
    FrameIdentityMismatch,
    /// A per-hop fee-parity conversion failed (§3.4 guard).
    FeeParity(FeeParityError),
}

impl core::fmt::Display for AssembleError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidField(name) => write!(formatter, "invalid field: {name}"),
            Self::ZeroAddress(name) => write!(formatter, "zero address: {name}"),
            Self::MinFinalBelowPrincipal => {
                write!(formatter, "minFinalAmount must be >= amountIn")
            }
            Self::VictimNotEip1559 => {
                write!(formatter, "victim_raw_tx must be a signed EIP-1559 envelope")
            }
            Self::VictimHashMismatch => {
                write!(formatter, "victim hash does not match victim_raw_tx")
            }
            Self::VictimNotBoundToPlan => {
                write!(formatter, "victim hash does not match plan.victim")
            }
            Self::VictimPriorityFeeMismatch => {
                write!(formatter, "expected victim priority fee does not match envelope")
            }
            Self::MaxFeeBelowVictimPriority => {
                write!(formatter, "max_fee_per_gas must be >= victim priority fee")
            }
            Self::DigestMismatch => {
                write!(formatter, "plan digest does not match its fields (field/fee tamper)")
            }
            Self::FrameIdentityMismatch => {
                write!(formatter, "plan frame identity does not match the current frame")
            }
            Self::FeeParity(error) => write!(formatter, "fee parity: {error}"),
        }
    }
}

impl core::error::Error for AssembleError {}

impl From<FeeParityError> for AssembleError {
    fn from(error: FeeParityError) -> Self {
        Self::FeeParity(error)
    }
}

fn require_non_zero(name: &'static str, address: Address) -> Result<Address, AssembleError> {
    if address.is_zero() { Err(AssembleError::ZeroAddress(name)) } else { Ok(address) }
}

/// Parse the victim envelope and return its `max_priority_fee_per_gas`. Accepts
/// only a byte-aligned EIP-1559 (`0x02`) transaction, matching the TS guard.
fn victim_priority_fee(victim_raw_tx: &[u8]) -> Result<u128, AssembleError> {
    if victim_raw_tx.first() != Some(&0x02) {
        return Err(AssembleError::VictimNotEip1559);
    }
    let mut slice: &[u8] = victim_raw_tx;
    let envelope =
        TxEnvelope::decode_2718(&mut slice).map_err(|_| AssembleError::VictimNotEip1559)?;
    if !slice.is_empty() {
        return Err(AssembleError::VictimNotEip1559);
    }
    match envelope {
        TxEnvelope::Eip1559(signed) => Ok(signed.tx().max_priority_fee_per_gas),
        _ => Err(AssembleError::VictimNotEip1559),
    }
}

/// Build the `SwapHop` for route index `index` from the plan route and hop params.
///
/// R8: the ABI `feeBps` is derived SOLELY from the carried, digest-bound
/// `BackrunHop::fee_pips` — never from a caller-supplied value.
fn build_swap_hop(
    plan: &BackrunPlan,
    index: usize,
    params: HopExecutionParams,
    label: &'static str,
) -> Result<SwapHop, AssembleError> {
    let hop = &plan.route[index];
    let fee_bps = fee_bps_for_executor(hop.protocol, hop.fee_pips)?;
    Ok(SwapHop {
        adapter: require_non_zero(label, params.adapter)?,
        pool: require_non_zero("hop.pool", hop.pool)?,
        tokenIn: require_non_zero("hop.tokenIn", hop.token_in)?,
        tokenOut: require_non_zero("hop.tokenOut", hop.token_out)?,
        feeBps: alloy_primitives::aliases::U24::from(fee_bps),
        minAmountOut: params.min_amount_out,
    })
}

/// Two independent, fail-closed pre-emit gates:
///
/// 1. **Field integrity** — recompute the plan's self-excluding digest and compare
///    it to the stored digest. A tampered field OR a tampered `fee_pips` (now part
///    of the digest preimage, R8) flips the digest and aborts.
/// 2. **Frame identity** — compare the plan's FULL 5-field `MeasurementContext`
///    `{parent_hash, block_number, predecessor_index, payload_id, victim}` against
///    the TRUSTED current frame (`AssembleInput::current_frame`, sourced from the
///    live `ProcessedFrame`, NOT from the plan). A stale or wrong-parent/generation
///    plan is internally digest-valid, so this is a SEPARATE gate from (1); a
///    mismatch — including a same-parent but stale `predecessor_index`/`payload_id`
///    generation, OR a plan whose `victim` is not the current frame's victim —
///    aborts.
///
/// `victim` MUST be bound here: the raw-victim↔plan check in
/// [`assemble_unsigned_atomic_tx`] only proves the plan is self-consistent for ITS
/// OWN victim, NOT that that victim is the current frame's victim. A digest-valid
/// plan for victim B (with a raw tx for B) whose 4 frame fields happen to match a
/// current frame targeting victim A would otherwise emit against the wrong victim.
fn enforce_plan_integrity_and_frame(input: &AssembleInput<'_>) -> Result<(), AssembleError> {
    MeasurementEncoder::validate(input.plan).map_err(|_| AssembleError::DigestMismatch)?;
    let plan = input.plan;
    let frame = &input.current_frame;
    if plan.parent_hash != frame.parent_hash
        || plan.block_number != frame.block_number
        || plan.predecessor_index != frame.predecessor_index
        || plan.payload_id != frame.payload_id
        || plan.victim != frame.victim
    {
        return Err(AssembleError::FrameIdentityMismatch);
    }
    Ok(())
}

/// Encode `executeBlinkOfaAtomic` calldata for the plan. Public so the parity
/// test can byte-compare against the TS `encodeFunctionData` output.
///
/// No calldata is produced until BOTH pre-emit gates pass
/// ([`enforce_plan_integrity_and_frame`]): a tampered/stale plan yields an error,
/// never bytes.
pub fn encode_executor_calldata(input: &AssembleInput<'_>) -> Result<Vec<u8>, AssembleError> {
    enforce_plan_integrity_and_frame(input)?;
    let plan = input.plan;
    if plan.amount_in.is_zero() {
        return Err(AssembleError::InvalidField("amountIn"));
    }
    if plan.amount_out.is_zero() {
        return Err(AssembleError::InvalidField("minFinalAmount"));
    }
    if plan.amount_out < plan.amount_in {
        return Err(AssembleError::MinFinalBelowPrincipal);
    }
    if input.valid_until_block == 0 {
        return Err(AssembleError::InvalidField("validUntilBlock"));
    }
    let first_hop = build_swap_hop(plan, 0, input.hops[0], "firstHop.adapter")?;
    let second_hop = build_swap_hop(plan, 1, input.hops[1], "secondHop.adapter")?;
    let call = executeBlinkOfaAtomicCall {
        firstHop: first_hop,
        secondHop: second_hop,
        amountIn: plan.amount_in,
        minFinalAmount: plan.amount_out,
        validUntilBlock: U256::from(input.valid_until_block),
    };
    Ok(call.abi_encode())
}

/// Assemble the unsigned rung-1 backrun transaction and its dummy serialization.
pub fn assemble_unsigned_atomic_tx(
    input: &AssembleInput<'_>,
) -> Result<UnsignedAtomicTx, AssembleError> {
    if input.chain_id == 0 {
        return Err(AssembleError::InvalidField("chainId"));
    }
    if input.gas == 0 {
        return Err(AssembleError::InvalidField("gas"));
    }
    if input.max_fee_per_gas == 0 {
        return Err(AssembleError::InvalidField("maxFeePerGas"));
    }
    let executor = require_non_zero("executorAddress", input.executor)?;

    // Bind and cross-check the victim envelope.
    let computed_hash = keccak256(input.victim_raw_tx);
    if computed_hash != input.victim_tx_hash {
        return Err(AssembleError::VictimHashMismatch);
    }
    if input.victim_tx_hash != input.plan.victim {
        return Err(AssembleError::VictimNotBoundToPlan);
    }
    let victim_priority = victim_priority_fee(input.victim_raw_tx)?;
    if let Some(expected) = input.expected_victim_priority_fee
        && expected != victim_priority
    {
        return Err(AssembleError::VictimPriorityFeeMismatch);
    }
    if input.max_fee_per_gas < victim_priority {
        return Err(AssembleError::MaxFeeBelowVictimPriority);
    }

    let calldata = encode_executor_calldata(input)?;
    let unsigned_tx = alloy_consensus::TxEip1559 {
        chain_id: input.chain_id,
        nonce: input.nonce,
        gas_limit: input.gas,
        max_fee_per_gas: input.max_fee_per_gas,
        // Inclusion channel: the backrun priority fee equals the victim's.
        max_priority_fee_per_gas: victim_priority,
        to: TxKind::Call(executor),
        value: U256::ZERO,
        access_list: AccessList::default(),
        input: Bytes::from(calldata),
    };

    // Serialize with the fixed invalid dummy signature — non-broadcastable.
    let dummy_signed = unsigned_tx.clone().into_signed(dummy_signature());
    let dummy_signed_raw_tx = dummy_signed.encoded_2718();

    Ok(UnsignedAtomicTx {
        kind: UNSIGNED_ATOMIC_TX_KIND,
        unsigned_tx,
        dummy_signed_raw_tx,
        non_broadcastable: true,
        signature_kind: DUMMY_SIGNATURE_KIND,
        target_tx_hash: computed_hash,
        victim_max_priority_fee_per_gas: victim_priority,
    })
}

// -- B3-arm assembler-only validated witness ---------------------------------

/// A validated unsigned atomic backrun tx whose bytes are bound, by the ONLY
/// constructor [`assemble_validated`], to the exact plan/frame the assembler
/// verified. Every field is private and captured from an authoritative source:
/// the tx bytes come from [`assemble_unsigned_atomic_tx`]; the executor is
/// extracted from the tx `TxKind::Call` target (a `Create` is fail-closed
/// rejected); the victim/amount/digest come from the digest- and frame-checked
/// plan; `valid_until_block` is the deadline re-validated at egress. Because only
/// the assembler can build one, downstream witness code cannot pair arbitrary tx
/// bytes with a safe id. (`chain_id`/`nonce`/`gas`/`priority` live inside the
/// signed `unsigned_tx` bytes and are not duplicated here.)
// NO `Clone`/`Copy`: this is the assembler-only LINEAR witness — duplicating it
// would let one validated set of tx bytes be paired with two candidates.
#[cfg(feature = "arm")]
#[derive(Debug)]
pub struct ValidatedUnsignedAtomicTx {
    unsigned_tx: alloy_consensus::TxEip1559,
    victim: B256,
    plan_digest: B256,
    amount: U256,
    executor: Address,
    valid_until_block: u64,
}

#[cfg(feature = "arm")]
impl ValidatedUnsignedAtomicTx {
    /// The unsigned EIP-1559 backrun to be signed by the arm custody path.
    pub(crate) const fn unsigned_tx(&self) -> &alloy_consensus::TxEip1559 {
        &self.unsigned_tx
    }
    /// Bound victim transaction hash.
    pub(crate) const fn victim(&self) -> B256 {
        self.victim
    }
    /// The digest of the plan these bytes were assembled from.
    pub(crate) const fn plan_digest(&self) -> B256 {
        self.plan_digest
    }
    /// The plan principal (`amount_in`) bound into the calldata.
    pub(crate) const fn amount(&self) -> U256 {
        self.amount
    }
    /// The executor (backrun `to`) extracted from the tx.
    pub(crate) const fn executor(&self) -> Address {
        self.executor
    }
    /// The executor `validUntilBlock` deadline (re-validated at egress).
    pub(crate) const fn valid_until_block(&self) -> u64 {
        self.valid_until_block
    }
}

/// The ONLY constructor of [`ValidatedUnsignedAtomicTx`]. Runs the full
/// fail-closed [`assemble_unsigned_atomic_tx`] (digest + frame + victim binding +
/// fee parity) and then captures the per-field authoritative values alongside the
/// tx bytes. A tx whose `to` is not a `Call` (a contract creation) is rejected.
#[cfg(feature = "arm")]
pub fn assemble_validated(
    input: &AssembleInput<'_>,
) -> Result<ValidatedUnsignedAtomicTx, AssembleError> {
    // The backrun MUST be on Base (8453): both the policy input AND the assembled
    // signed-tx chain id, so a non-Base tx can never bind to a Base claim/executor.
    const CHAIN_ID_BASE: u64 = 8453;
    if input.chain_id != CHAIN_ID_BASE {
        return Err(AssembleError::InvalidField("chainId"));
    }
    let assembled = assemble_unsigned_atomic_tx(input)?;
    if assembled.unsigned_tx.chain_id != CHAIN_ID_BASE {
        return Err(AssembleError::InvalidField("chainId"));
    }
    let executor = match assembled.unsigned_tx.to {
        TxKind::Call(address) => address,
        TxKind::Create => return Err(AssembleError::InvalidField("executorKind")),
    };
    Ok(ValidatedUnsignedAtomicTx {
        victim: assembled.target_tx_hash,
        plan_digest: input.plan.digest.0,
        amount: input.plan.amount_in,
        executor,
        valid_until_block: input.valid_until_block,
        unsigned_tx: assembled.unsigned_tx,
    })
}

// -- Two-channel dummy assembly (inclusion + attribution) --------------------

/// A reference to a transaction inside the attribution bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleTxRef {
    /// The victim referenced by its 32-byte transaction hash (attribution slot 0).
    Hash(B256),
    /// The dummy backrun raw EIP-1559 envelope bytes (attribution slot 1).
    Raw(Vec<u8>),
}

/// The Blink `eth_sendBundle` attribution channel (struct only — never sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionBundle {
    /// The JSON-RPC method name this struct models.
    pub method: &'static str,
    /// `[victim_tx_hash, dummy_raw_backrun]` — the victim hash MUST be first.
    pub txs: [BundleTxRef; 2],
    /// The bid, always 0 (Blink OFA attribution ignores it).
    pub bid_wei: U256,
}

/// The two Blink OFA channels for a dummy backrun (struct/serialization only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwoChannelDummyAssembly {
    /// Inclusion channel: `[victim_raw_tx, dummy_raw_backrun]`
    /// (`eth_sendRawTransaction` shape, backrun priority fee == victim's).
    pub direct: [Vec<u8>; 2],
    /// Attribution channel: `eth_sendBundle[victim_tx_hash, dummy_raw_backrun]`.
    pub attribution: AttributionBundle,
}

/// Inputs to [`build_two_channel_dummy_assembly`].
#[derive(Debug, Clone, Copy)]
pub struct TwoChannelInput<'a> {
    /// The signed victim EIP-1559 envelope.
    pub victim_raw_tx: &'a [u8],
    /// The victim transaction hash.
    pub victim_tx_hash: B256,
    /// The dummy-signed backrun raw envelope from [`assemble_unsigned_atomic_tx`].
    pub dummy_raw_backrun: &'a [u8],
}

/// Assemble the inclusion + attribution channels, validating that the backrun is
/// the fixed dummy envelope and its priority fee matches the victim. Mirrors the
/// TS `buildBlinkTwoChannelDummyAssembly`.
pub fn build_two_channel_dummy_assembly(
    input: &TwoChannelInput<'_>,
) -> Result<TwoChannelDummyAssembly, AssembleError> {
    if input.victim_raw_tx.first() != Some(&0x02) || input.dummy_raw_backrun.first() != Some(&0x02)
    {
        return Err(AssembleError::VictimNotEip1559);
    }
    if keccak256(input.victim_raw_tx) != input.victim_tx_hash {
        return Err(AssembleError::VictimHashMismatch);
    }
    let victim_priority = victim_priority_fee(input.victim_raw_tx)?;

    // Decode the backrun and require the fixed dummy signature + matching priority.
    let mut backrun_slice: &[u8] = input.dummy_raw_backrun;
    let backrun =
        TxEnvelope::decode_2718(&mut backrun_slice).map_err(|_| AssembleError::VictimNotEip1559)?;
    let TxEnvelope::Eip1559(backrun_signed) = backrun else {
        return Err(AssembleError::VictimNotEip1559);
    };
    let signature = backrun_signed.signature();
    let dummy = dummy_signature();
    if signature.r() != dummy.r() || signature.s() != dummy.s() || signature.v() != dummy.v() {
        return Err(AssembleError::InvalidField("dummy backrun signature"));
    }
    if backrun_signed.tx().max_priority_fee_per_gas != victim_priority {
        return Err(AssembleError::VictimPriorityFeeMismatch);
    }

    Ok(TwoChannelDummyAssembly {
        direct: [input.victim_raw_tx.to_vec(), input.dummy_raw_backrun.to_vec()],
        attribution: AttributionBundle {
            method: "eth_sendBundle",
            txs: [
                BundleTxRef::Hash(input.victim_tx_hash),
                BundleTxRef::Raw(input.dummy_raw_backrun.to_vec()),
            ],
            bid_wei: U256::ZERO,
        },
    })
}

// -- Blocked boundaries ------------------------------------------------------

/// A crossed rung boundary. Mirrors the TS `*Blocked()` throwing helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedBoundary {
    /// Rung-2 boundary: the rung-1 assembler never signs.
    Sign,
    /// Rung-3 boundary: real submission is unavailable.
    Submit,
}

impl core::fmt::Display for BlockedBoundary {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sign => {
                write!(
                    formatter,
                    "Rung 2 boundary: transaction signing is intentionally unavailable"
                )
            }
            Self::Submit => write!(
                formatter,
                "Rung 3 boundary: transaction submission is intentionally unavailable"
            ),
        }
    }
}

impl core::error::Error for BlockedBoundary {}

/// Rung-2 boundary stub: the rung-1 assembler does not sign. Real signing is the
/// ephemeral-only [`crate::signer`] path. Always returns [`BlockedBoundary::Sign`].
pub const fn sign_blocked() -> Result<core::convert::Infallible, BlockedBoundary> {
    Err(BlockedBoundary::Sign)
}

/// Rung-3 boundary stub: real transaction/bundle submission is intentionally
/// unavailable. Always returns [`BlockedBoundary::Submit`].
pub const fn submit_blocked() -> Result<core::convert::Infallible, BlockedBoundary> {
    Err(BlockedBoundary::Submit)
}
