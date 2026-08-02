//! Node-local authority derivation for bounded, unsigned T4b transaction-shape observation.

use std::{
    fmt::{self, Debug},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use alloy_consensus::{Header, Sealed, SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::{eip2718::Decodable2718, eip2930::AccessList};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, b256, keccak256};
use base_mev_trader::{
    BackrunHop, CandidateAssemblyView, CanonicalL1FeeEvidenceV2, ExactProtocol, MeasurementContext,
    MeasurementEncoder, PreparedPoolState, PriorityEconomicsCountersV2, PriorityEconomicsLedgerV2,
    PriorityEconomicsV2, SelectedRouteEvidenceV2, SnapshotHandle, WETH,
};

#[cfg(feature = "t4e-handoff")]
use crate::PriorityEconomicsReceipt;
use crate::{
    CanonicalL1EnvelopeEvidence, PriorityEconomicsAuthority,
    calldata::AtomicCalldataEncoder,
    economics::{PriorityFilterInput, evaluate},
    fee::fee_bps_for_executor,
};
#[cfg(feature = "t4d-bridge")]
mod bridge;
#[cfg(feature = "t4d-bridge")]
pub use bridge::{
    AdapterAwareProofBindings, BridgeError, InstalledSubmissionBridge, SealedUnsignedCandidate,
};
#[cfg(feature = "t4e-handoff")]
pub use bridge::{BridgeConversionSeal, T4eCandidateHandoff, T4eHandoffError};

const CHAIN_ID_BASE: u64 = 8_453;
const T4B_EXECUTOR_GAS_LIMIT: u64 = 3_000_000;
const T4B_VALID_WINDOW_BLOCKS: u64 = 0;

const EXECUTOR_ADDRESS: Address = address!("1810cbFA042e8199121021F056Afe8B31028CF55");
const EXECUTOR_RUNTIME_HASH: B256 =
    b256!("cc7e119d458f147a9baab30f51d4ef7fbfe6eef48377988d480f6e7b693e671d");
const EXECUTOR_SENDER: Address = address!("98e1e2A84557D49496D1BFE31EA7b5a6C59FD0f9");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedAbiHop {
    adapter: Address,
    pool: Address,
    token_in: Address,
    token_out: Address,
    fee_bps: u32,
    min_amount_out: U256,
    funding_target: Address,
}

impl ValidatedAbiHop {
    pub(crate) const fn parts(&self) -> (Address, Address, Address, Address, u32, U256, Address) {
        (
            self.adapter,
            self.pool,
            self.token_in,
            self.token_out,
            self.fee_bps,
            self.min_amount_out,
            self.funding_target,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedAtomicCall {
    hops: [ValidatedAbiHop; 2],
    amount_in: U256,
    min_final_amount: U256,
    valid_until_block: u64,
}

impl ValidatedAtomicCall {
    pub(crate) const fn hops(&self) -> [&ValidatedAbiHop; 2] {
        [&self.hops[0], &self.hops[1]]
    }

    pub(crate) const fn amount_in(&self) -> U256 {
        self.amount_in
    }

    pub(crate) const fn min_final_amount(&self) -> U256 {
        self.min_final_amount
    }

    pub(crate) const fn valid_until_block(&self) -> u64 {
        self.valid_until_block
    }
}

/// One compile-pinned deployed contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedContractIdentity {
    address: Address,
    runtime_hash: B256,
}

impl DeployedContractIdentity {
    /// Returns the pinned contract address.
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Returns the pinned deployed runtime-code hash.
    pub const fn runtime_hash(&self) -> B256 {
        self.runtime_hash
    }
}

/// Total Base-mainnet mapping from every supported protocol to its deployed adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolAdapterMapping {
    uniswap_v2: DeployedContractIdentity,
    uniswap_v3: DeployedContractIdentity,
    aerodrome: DeployedContractIdentity,
}

impl ProtocolAdapterMapping {
    /// Returns the exact reviewed Base-mainnet deployment pins.
    pub const fn base_mainnet_pins() -> Self {
        Self {
            uniswap_v2: DeployedContractIdentity {
                address: address!("17314D6F1B7A7A67b91A6131E3AE635AF90bAe1b"),
                runtime_hash: b256!(
                    "75f81d350eea433778932277055e2fb493f9954c307df0c0a7613418aa0ca2ae"
                ),
            },
            uniswap_v3: DeployedContractIdentity {
                address: address!("D73a2ACbd855AdA5bdF950E3D3796E0557504DfF"),
                runtime_hash: b256!(
                    "dd184f8bc9680971b835c0f05c4f5986845ba888db0857f04eb3c8cd4e44d93c"
                ),
            },
            aerodrome: DeployedContractIdentity {
                address: address!("6a2242f52Db5aC8d6631AC7244Bb7fa370454F24"),
                runtime_hash: b256!(
                    "426950cd6948988dc7de442de3a3efcbc588670e29856b4bbd3cac70c374b8f5"
                ),
            },
        }
    }

    /// Resolves every supported protocol without a fallback mapping.
    pub const fn resolve(&self, protocol: ExactProtocol) -> &DeployedContractIdentity {
        match protocol {
            ExactProtocol::UniswapV2 => &self.uniswap_v2,
            ExactProtocol::UniswapV3 => &self.uniswap_v3,
            ExactProtocol::AerodromeVolatile | ExactProtocol::AerodromeStable => &self.aerodrome,
        }
    }
}

/// Executor, adapters, and sender proven against one committed parent.
#[derive(Debug, PartialEq, Eq)]
pub struct InstalledExecutionIdentity {
    executor: DeployedContractIdentity,
    adapters: ProtocolAdapterMapping,
    sender: Address,
    validated_parent: B256,
}

impl InstalledExecutionIdentity {
    /// Returns the executor identity.
    pub const fn executor(&self) -> &DeployedContractIdentity {
        &self.executor
    }

    /// Returns the total adapter mapping.
    pub const fn adapters(&self) -> &ProtocolAdapterMapping {
        &self.adapters
    }

    /// Returns the compile-pinned public sender address.
    pub const fn sender(&self) -> Address {
        self.sender
    }

    /// Returns the committed parent against which all code was checked.
    pub const fn validated_parent(&self) -> B256 {
        self.validated_parent
    }
}

/// One batch read from a single hash-pinned committed-state session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxAuthorityStateRead {
    parent_hash: B256,
    committed_sender_nonce: Option<u64>,
    runtime_codes: [Option<Bytes>; 4],
}

impl TxAuthorityStateRead {
    /// Constructs the result of one in-process hash-pinned state session.
    pub const fn new(
        parent_hash: B256,
        committed_sender_nonce: Option<u64>,
        runtime_codes: [Option<Bytes>; 4],
    ) -> Self {
        Self { parent_hash, committed_sender_nonce, runtime_codes }
    }

    /// Returns the parent hash pin used for every value in this batch.
    pub const fn parent_hash(&self) -> B256 {
        self.parent_hash
    }

    /// Returns the committed sender nonce, or `None` for an absent account.
    pub const fn committed_sender_nonce(&self) -> Option<u64> {
        self.committed_sender_nonce
    }

    /// Returns runtime code in the exact requested contract-address order.
    pub const fn runtime_codes(&self) -> &[Option<Bytes>; 4] {
        &self.runtime_codes
    }
}

/// Opaque node-owned witness for the exact captured pending snapshot.
pub trait SnapshotFreshnessToken: Debug + Send + Sync {
    /// Returns whether the exact captured pending record is still authoritative.
    fn is_current(&self) -> Result<bool, TxAuthorityNodeError>;
}

/// Failures exposed by a read-only in-process node adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxAuthorityNodeError {
    /// The hash-pinned state view was unavailable.
    Unavailable,
    /// The node view returned internally incoherent data.
    Incoherent,
}

impl core::fmt::Display for TxAuthorityNodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("node authority unavailable"),
            Self::Incoherent => formatter.write_str("node authority incoherent"),
        }
    }
}

impl core::error::Error for TxAuthorityNodeError {}

/// Minimal read-only in-process node authority required by T4b.
pub trait TxAuthorityNodeView: Debug + Send + Sync {
    /// Returns the configured chain id.
    fn chain_id(&self) -> Result<u64, TxAuthorityNodeError>;
    /// Returns the current authoritative committed parent.
    fn current_parent_hash(&self) -> Result<B256, TxAuthorityNodeError>;
    /// Reads sender nonce and all four contract codes from one hash-pinned state session.
    fn read_state_at_parent(
        &self,
        parent_hash: B256,
        sender: Address,
        contracts: [Address; 4],
    ) -> Result<TxAuthorityStateRead, TxAuthorityNodeError>;
    /// Revalidates that the captured pending snapshot is still authoritative.
    fn is_current_authoritative(
        &self,
        snapshot: &SnapshotHandle,
    ) -> Result<bool, TxAuthorityNodeError>;
    /// Captures a non-retaining witness for drain-time exact-snapshot freshness.
    fn capture_snapshot_freshness(
        &self,
        snapshot: &SnapshotHandle,
    ) -> Result<Box<dyn SnapshotFreshnessToken>, TxAuthorityNodeError>;
}

/// Typed fail-closed reason for producing no unsigned T4b shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxAuthorityError {
    /// Plan digest or full frame identity was rejected.
    PlanOrFrameRejected,
    /// Raw victim type, hash, chain, or fee fields were rejected.
    FeeAuthorityRejected,
    /// Candidate economics were missing, invalid, stale, overflowed, or failed conservation.
    PriorityEconomicsRejected,
    /// Finalized economics could not be retained in the bounded production ledger.
    PriorityEconomicsLedgerUnavailable,
    /// Same-frame route re-quote was missing, ambiguous, or unequal to the plan.
    RequoteRejected,
    /// Executor or adapter deployment identity was unavailable or mismatched.
    DeploymentIdentityRejected,
    /// Committed or pending nonce authority was unavailable or incoherent.
    NonceWitnessUnavailable,
    /// Another linear observation currently owns the guard.
    ObservationBusy,
    /// Nonce or snapshot authority changed after assembly.
    NonceWitnessStaleBeforePublish,
    /// The captured snapshot was replaced before the bounded observation drained.
    SnapshotStaleAtDrain,
    /// The shared cancellation token stopped the operation.
    Cancelled,
    /// The shared deadline elapsed.
    DeadlineNoShape,
    /// Calldata or unsigned-field self-validation failed.
    AssemblyRejected,
}

impl core::fmt::Display for TxAuthorityError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "T4b transaction authority rejected: {self:?}")
    }
}

impl core::error::Error for TxAuthorityError {}

/// Header-derived Beryl EVM inputs checked during preparation.
#[derive(Debug)]
pub struct CheckedBerylEnvInputs {
    chain_id: u64,
    block_number: u64,
    timestamp: u64,
    gas_limit: u64,
    base_fee_per_gas: u64,
    prev_randao: B256,
    excess_blob_gas: Option<u64>,
}

impl CheckedBerylEnvInputs {
    /// Returns the chain id.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    /// Returns the block number.
    pub const fn block_number(&self) -> u64 {
        self.block_number
    }
    /// Returns the timestamp.
    pub const fn timestamp(&self) -> u64 {
        self.timestamp
    }
    /// Returns the block gas limit.
    pub const fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
    /// Returns the base fee.
    pub const fn base_fee_per_gas(&self) -> u64 {
        self.base_fee_per_gas
    }
    /// Returns the checked prevrandao.
    pub const fn prev_randao(&self) -> B256 {
        self.prev_randao
    }
    /// Returns excess blob gas when present.
    pub const fn excess_blob_gas(&self) -> Option<u64> {
        self.excess_blob_gas
    }
}

/// Checked executor and route-adapter deployments at one parent.
#[derive(Debug)]
pub struct DeploymentWitness {
    validated_parent: B256,
    executor: DeployedContractIdentity,
    route_adapters: [DeployedContractIdentity; 2],
}

impl DeploymentWitness {
    /// Returns the validated parent.
    pub const fn validated_parent(&self) -> B256 {
        self.validated_parent
    }
    /// Returns the executor identity.
    pub const fn executor(&self) -> &DeployedContractIdentity {
        &self.executor
    }
    /// Returns ordered route adapter identities.
    pub const fn route_adapters(&self) -> [&DeployedContractIdentity; 2] {
        [&self.route_adapters[0], &self.route_adapters[1]]
    }
}

/// Checked committed, overlay, and transaction nonce values.
#[derive(Debug)]
pub struct NonceWitness {
    sender: Address,
    parent_hash: B256,
    committed_nonce: u64,
    pending_overlay_nonce: Option<u64>,
    shape_nonce: u64,
}

impl NonceWitness {
    /// Returns the sender.
    pub const fn sender(&self) -> Address {
        self.sender
    }
    /// Returns the parent hash.
    pub const fn parent_hash(&self) -> B256 {
        self.parent_hash
    }
    /// Returns the committed nonce.
    pub const fn committed_nonce(&self) -> u64 {
        self.committed_nonce
    }
    /// Returns the pending overlay nonce.
    pub const fn pending_overlay_nonce(&self) -> Option<u64> {
        self.pending_overlay_nonce
    }
    /// Returns the selected shape nonce.
    pub const fn shape_nonce(&self) -> u64 {
        self.shape_nonce
    }
}

/// Immutable checked snapshot-freshness summary.
#[derive(Debug)]
pub struct FreshnessWitness {
    parent_hash: B256,
    snapshot_parent_hash: B256,
    valid_until_block: u64,
    snapshot_identity_digest: B256,
}

impl FreshnessWitness {
    /// Returns the parent hash.
    pub const fn parent_hash(&self) -> B256 {
        self.parent_hash
    }
    /// Returns the captured snapshot parent hash.
    pub const fn snapshot_parent_hash(&self) -> B256 {
        self.snapshot_parent_hash
    }
    /// Returns the last authorized block.
    pub const fn valid_until_block(&self) -> u64 {
        self.valid_until_block
    }
    /// Returns the snapshot identity digest.
    pub const fn snapshot_identity_digest(&self) -> B256 {
        self.snapshot_identity_digest
    }
}

/// Owned checked bindings retained by the linear candidate.
#[derive(Debug)]
pub struct CheckedBindings {
    frame: MeasurementContext,
    parent_header: Sealed<Header>,
    header_identity_digest: B256,
    beryl_env: CheckedBerylEnvInputs,
    sender: Address,
    kickback_recipient: Address,
    route_hops: [BackrunHop; 2],
    route_pools: [Address; 2],
    route_tokens: [Address; 3],
    header_coinbase: Address,
    deployment: DeploymentWitness,
    nonce: NonceWitness,
    freshness: FreshnessWitness,
    frame_digest: B256,
    plan_digest: B256,
    route_digest: B256,
    shape_digest: B256,
    overlay_digest: B256,
    order_digest: B256,
    state_digest: B256,
    access_digest: B256,
    unsigned_signing_hash: B256,
}

/// Borrowed read-only projection available only inside one adapter entry.
#[derive(Debug)]
pub struct CheckedBindingsView<'a> {
    frame: &'a MeasurementContext,
    parent_header: &'a Sealed<Header>,
    header_identity_digest: B256,
    beryl_env: &'a CheckedBerylEnvInputs,
    sender: Address,
    kickback_recipient: Address,
    route_hops: &'a [BackrunHop; 2],
    route_pools: [Address; 2],
    route_tokens: [Address; 3],
    header_coinbase: Address,
    deployment: &'a DeploymentWitness,
    nonce: &'a NonceWitness,
    freshness: &'a FreshnessWitness,
    frame_digest: B256,
    plan_digest: B256,
    route_digest: B256,
    shape_digest: B256,
    overlay_digest: B256,
    order_digest: B256,
    state_digest: B256,
    access_digest: B256,
    unsigned_signing_hash: B256,
}

impl<'a> CheckedBindingsView<'a> {
    /// Returns the frame.
    pub const fn frame(&self) -> &MeasurementContext {
        self.frame
    }
    /// Returns the parent hash.
    pub const fn parent_hash(&self) -> B256 {
        self.frame.parent_hash
    }
    /// Returns the sealed pending header.
    pub const fn parent_header(&self) -> &Sealed<Header> {
        self.parent_header
    }
    /// Returns the sealed-header identity digest.
    pub const fn header_identity_digest(&self) -> B256 {
        self.header_identity_digest
    }
    /// Returns checked Beryl environment inputs.
    pub const fn beryl_env(&self) -> &CheckedBerylEnvInputs {
        self.beryl_env
    }
    /// Returns the sender.
    pub const fn sender(&self) -> Address {
        self.sender
    }
    /// Returns the kickback recipient.
    pub const fn kickback_recipient(&self) -> Address {
        self.kickback_recipient
    }
    /// Returns ordered route hops.
    pub const fn route_hops(&self) -> &[BackrunHop; 2] {
        self.route_hops
    }
    /// Returns ordered route pools.
    pub const fn route_pools(&self) -> [Address; 2] {
        self.route_pools
    }
    /// Returns ordered route tokens.
    pub const fn route_tokens(&self) -> [Address; 3] {
        self.route_tokens
    }
    /// Returns ordered route protocols.
    pub const fn route_protocols(&self) -> [ExactProtocol; 2] {
        [self.route_hops[0].protocol, self.route_hops[1].protocol]
    }
    /// Returns ordered resolved adapters.
    pub const fn resolved_adapters(&self) -> [&DeployedContractIdentity; 2] {
        self.deployment.route_adapters()
    }
    /// Returns the executor.
    pub const fn executor(&self) -> &DeployedContractIdentity {
        self.deployment.executor()
    }
    /// Returns the header coinbase.
    pub const fn header_coinbase(&self) -> Address {
        self.header_coinbase
    }
    /// Returns the deployment witness.
    pub const fn deployment_witness(&self) -> &DeploymentWitness {
        self.deployment
    }
    /// Returns the nonce witness.
    pub const fn nonce_witness(&self) -> &NonceWitness {
        self.nonce
    }
    /// Returns the freshness witness.
    pub const fn freshness_witness(&self) -> &FreshnessWitness {
        self.freshness
    }
    /// Returns the frame digest.
    pub const fn frame_digest(&self) -> B256 {
        self.frame_digest
    }
    /// Returns the plan digest.
    pub const fn plan_digest(&self) -> B256 {
        self.plan_digest
    }
    /// Returns the route digest.
    pub const fn route_digest(&self) -> B256 {
        self.route_digest
    }
    /// Returns the shape digest.
    pub const fn shape_digest(&self) -> B256 {
        self.shape_digest
    }
    /// Returns the overlay digest.
    pub const fn overlay_digest(&self) -> B256 {
        self.overlay_digest
    }
    /// Returns the order digest.
    pub const fn order_digest(&self) -> B256 {
        self.order_digest
    }
    /// Returns the state digest.
    pub const fn state_digest(&self) -> B256 {
        self.state_digest
    }
    /// Returns the access digest.
    pub const fn access_digest(&self) -> B256 {
        self.access_digest
    }
    /// Returns the unsigned signing hash.
    pub const fn unsigned_signing_hash(&self) -> B256 {
        self.unsigned_signing_hash
    }
}

/// Owned candidate execution evidence; no transaction or bindings borrow is retained.
#[derive(Debug)]
pub struct CandidateEconomicsEvidence {
    authority: PriorityEconomicsAuthority,
    weth_delta: U256,
    canonical_l1: CanonicalL1EnvelopeEvidence,
    frame_digest: B256,
    plan_digest: B256,
    route_digest: B256,
    shape_digest: B256,
    overlay_digest: B256,
    order_digest: B256,
    state_digest: B256,
    access_digest: B256,
    unsigned_signing_hash: B256,
    deployment_parent: B256,
    nonce_parent: B256,
    freshness_parent: B256,
    snapshot_identity_digest: B256,
}

impl CandidateEconomicsEvidence {
    /// Derives the nonzero executed recipient WETH increase from audited balances.
    pub fn checked_weth_delta(pre_weth: U256, post_weth: U256) -> Result<U256, TxAuthorityError> {
        post_weth
            .checked_sub(pre_weth)
            .filter(|delta| !delta.is_zero())
            .ok_or(TxAuthorityError::PriorityEconomicsRejected)
    }
    /// Checks and copies adapter results into borrow-free evidence.
    pub fn try_new(
        authority: PriorityEconomicsAuthority,
        weth_delta: U256,
        canonical_l1: CanonicalL1EnvelopeEvidence,
        bindings: CheckedBindingsView<'_>,
    ) -> Result<Self, TxAuthorityError> {
        if authority.execution_gas_estimate().is_zero()
            || weth_delta.is_zero()
            || authority.l1_data_fee_wei() != canonical_l1.fee()
            || authority.base_fee_per_gas_wei()
                != U256::from(bindings.beryl_env().base_fee_per_gas())
            || authority.block() != bindings.beryl_env().block_number()
        {
            return Err(TxAuthorityError::PriorityEconomicsRejected);
        }
        Ok(Self {
            authority,
            weth_delta,
            canonical_l1,
            frame_digest: bindings.frame_digest(),
            plan_digest: bindings.plan_digest(),
            route_digest: bindings.route_digest(),
            shape_digest: bindings.shape_digest(),
            overlay_digest: bindings.overlay_digest(),
            order_digest: bindings.order_digest(),
            state_digest: bindings.state_digest(),
            access_digest: bindings.access_digest(),
            unsigned_signing_hash: bindings.unsigned_signing_hash(),
            deployment_parent: bindings.deployment_witness().validated_parent(),
            nonce_parent: bindings.nonce_witness().parent_hash(),
            freshness_parent: bindings.freshness_witness().parent_hash(),
            snapshot_identity_digest: bindings.freshness_witness().snapshot_identity_digest(),
        })
    }

    /// Returns the checked economics authority.
    pub const fn authority(&self) -> PriorityEconomicsAuthority {
        self.authority
    }
    /// Returns the observed recipient WETH increase.
    pub const fn weth_delta(&self) -> U256 {
        self.weth_delta
    }
    /// Returns raw-free canonical L1 evidence.
    pub const fn canonical_l1(&self) -> CanonicalL1EnvelopeEvidence {
        self.canonical_l1
    }
}

/// By-value adapter entry for exactly one candidate execution.
pub trait CandidateExecutionAdapter: Sized {
    /// Adapter-specific failure.
    type Error;

    /// Executes a candidate once and returns owned evidence.
    fn execute_candidate(
        self,
        request: TxAuthorityExecutionRequest<'_>,
    ) -> Result<CandidateEconomicsEvidence, Self::Error>;
}

/// Linear private request passed to one consumed adapter.
#[derive(Debug)]
pub struct TxAuthorityExecutionRequest<'a> {
    tx: &'a TxEip1559,
    bindings: &'a CheckedBindings,
}

impl<'a> TxAuthorityExecutionRequest<'a> {
    fn new_private(candidate: &'a PreEconomicsCandidate) -> Self {
        Self { tx: &candidate.unsigned_tx, bindings: &candidate.bindings }
    }

    /// Consumes the request into its only public decomposition.
    pub fn into_parts(self) -> TxAuthorityExecutionParts<'a> {
        TxAuthorityExecutionParts { tx: self.tx, bindings: self.bindings }
    }
}

/// Linear request parts with one further consuming decomposition.
#[derive(Debug)]
pub struct TxAuthorityExecutionParts<'a> {
    tx: &'a TxEip1559,
    bindings: &'a CheckedBindings,
}

impl<'a> TxAuthorityExecutionParts<'a> {
    /// Consumes parts into the checked transaction and bindings projection.
    pub fn into_tx_and_bindings(self) -> (&'a TxEip1559, CheckedBindingsView<'a>) {
        let bindings = self.bindings;
        (
            self.tx,
            CheckedBindingsView {
                frame: &bindings.frame,
                parent_header: &bindings.parent_header,
                header_identity_digest: bindings.header_identity_digest,
                beryl_env: &bindings.beryl_env,
                sender: bindings.sender,
                kickback_recipient: bindings.kickback_recipient,
                route_hops: &bindings.route_hops,
                route_pools: bindings.route_pools,
                route_tokens: bindings.route_tokens,
                header_coinbase: bindings.header_coinbase,
                deployment: &bindings.deployment,
                nonce: &bindings.nonce,
                freshness: &bindings.freshness,
                frame_digest: bindings.frame_digest,
                plan_digest: bindings.plan_digest,
                route_digest: bindings.route_digest,
                shape_digest: bindings.shape_digest,
                overlay_digest: bindings.overlay_digest,
                order_digest: bindings.order_digest,
                state_digest: bindings.state_digest,
                access_digest: bindings.access_digest,
                unsigned_signing_hash: bindings.unsigned_signing_hash,
            },
        )
    }
}

/// Candidate after shape preparation but before candidate execution economics.
#[derive(Debug)]
pub struct PreEconomicsCandidate {
    unsigned_tx: TxEip1559,
    amount: U256,
    gross_profit: U256,
    victim_priority: u128,
    victim_max_fee: u128,
    bindings: CheckedBindings,
    observation: UnsignedTxShapeObservation,
    execution: InstalledExecutionIdentity,
    observation_guard: MeasurementNonceGuard,
    snapshot_freshness: Box<dyn SnapshotFreshnessToken>,
    node: Arc<dyn TxAuthorityNodeView>,
}

impl PreEconomicsCandidate {
    /// Consumes this candidate and one adapter, permitting one execution entry.
    pub fn execute_once<A>(
        self,
        adapter: A,
    ) -> Result<EconomicsReadyCandidate, ExecuteOnceError<A::Error>>
    where
        A: CandidateExecutionAdapter,
    {
        let evidence = {
            let request = TxAuthorityExecutionRequest::new_private(&self);
            adapter.execute_candidate(request).map_err(ExecuteOnceError::Execution)?
        };
        Ok(EconomicsReadyCandidate { pre: self, evidence })
    }
}

/// Candidate carrying owned execution economics and awaiting freshness finalization.
#[derive(Debug)]
pub struct EconomicsReadyCandidate {
    pre: PreEconomicsCandidate,
    evidence: CandidateEconomicsEvidence,
}

/// Error from the single adapter entry.
#[derive(Debug, PartialEq, Eq)]
pub enum ExecuteOnceError<E> {
    /// The consumed adapter rejected execution.
    Execution(E),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotNonceWitness {
    sender: Address,
    parent_hash: B256,
    committed_nonce: u64,
    pending_overlay_nonce: Option<u64>,
    shape_nonce: u64,
}

#[derive(Debug)]
struct AuthorityAssemblyView<'a> {
    snapshot: &'a SnapshotHandle,
    frame: MeasurementContext,
    prepared: &'a [PreparedPoolState],
    plan: &'a base_mev_trader::BackrunPlan,
    victim_raw: &'a Bytes,
    probe: &'a base_mev_trader::CancellationProbe,
}

impl<'a> AuthorityAssemblyView<'a> {
    const fn from_candidate(view: &'a CandidateAssemblyView<'a>) -> Self {
        Self {
            snapshot: view.snapshot(),
            frame: *view.processed().measurement_context(),
            prepared: view.prepared(),
            plan: view.plan(),
            victim_raw: view.victim_raw(),
            probe: view.probe(),
        }
    }

    const fn snapshot(&self) -> &'a SnapshotHandle {
        self.snapshot
    }

    const fn prepared(&self) -> &'a [PreparedPoolState] {
        self.prepared
    }

    const fn plan(&self) -> &'a base_mev_trader::BackrunPlan {
        self.plan
    }

    const fn victim_raw(&self) -> &'a Bytes {
        self.victim_raw
    }

    const fn probe(&self) -> &'a base_mev_trader::CancellationProbe {
        self.probe
    }
}

#[derive(Debug)]
struct MeasurementNonceGuard {
    held: Arc<AtomicBool>,
}

impl Drop for MeasurementNonceGuard {
    fn drop(&mut self) {
        self.held.store(false, Ordering::Release);
    }
}

/// Bounded public summary of an unsigned transaction shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsignedTxShapeObservation {
    frame: MeasurementContext,
    victim: B256,
    plan_digest: B256,
    sender: Address,
    nonce: u64,
    chain_id: u64,
    executor: Address,
    hop_protocols: [ExactProtocol; 2],
    hop_adapters: [Address; 2],
    hop_runtime_hashes: [B256; 2],
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    base_fee: u128,
    valid_until_block: u64,
    unsigned_signing_hash: B256,
}

impl UnsignedTxShapeObservation {
    /// Returns the full frame identity.
    pub const fn frame(&self) -> MeasurementContext {
        self.frame
    }
    /// Returns the bound victim hash.
    pub const fn victim(&self) -> B256 {
        self.victim
    }
    /// Returns the measurement plan digest.
    pub const fn plan_digest(&self) -> B256 {
        self.plan_digest
    }
    /// Returns the public sender address.
    pub const fn sender(&self) -> Address {
        self.sender
    }
    /// Returns the snapshot-derived nonce shape.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }
    /// Returns the chain id.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    /// Returns the executor address.
    pub const fn executor(&self) -> Address {
        self.executor
    }
    /// Returns the two exact route protocols.
    pub const fn hop_protocols(&self) -> [ExactProtocol; 2] {
        self.hop_protocols
    }
    /// Returns the two mapped adapter addresses.
    pub const fn hop_adapters(&self) -> [Address; 2] {
        self.hop_adapters
    }
    /// Returns the two mapped adapter runtime hashes.
    pub const fn hop_runtime_hashes(&self) -> [B256; 2] {
        self.hop_runtime_hashes
    }
    /// Returns the fixed T4b gas limit.
    pub const fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
    /// Returns the derived maximum fee.
    pub const fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }
    /// Returns the victim-derived priority fee.
    pub const fn max_priority_fee_per_gas(&self) -> u128 {
        self.max_priority_fee_per_gas
    }
    /// Returns the snapshot base fee.
    pub const fn base_fee(&self) -> u128 {
        self.base_fee
    }
    /// Returns the exact current-block deadline.
    pub const fn valid_until_block(&self) -> u64 {
        self.valid_until_block
    }
    /// Returns the native unsigned EIP-1559 signing hash.
    pub const fn unsigned_signing_hash(&self) -> B256 {
        self.unsigned_signing_hash
    }
}

/// Linear unsigned-only output that retains the single-observation guard.
pub struct ValidatedUnsignedAtomicTx {
    unsigned_tx: TxEip1559,
    #[cfg(feature = "t4e-handoff")]
    amount: U256,
    #[cfg(feature = "t4e-handoff")]
    economics: PriorityEconomicsReceipt,
    priority_economics: PriorityEconomicsV2,
    observation: UnsignedTxShapeObservation,
    execution: InstalledExecutionIdentity,
    observation_guard: MeasurementNonceGuard,
    snapshot_freshness: Box<dyn SnapshotFreshnessToken>,
}

impl Debug for ValidatedUnsignedAtomicTx {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedUnsignedAtomicTx")
            .field("observation", &self.observation)
            .field("execution", &self.execution)
            .finish_non_exhaustive()
    }
}

impl ValidatedUnsignedAtomicTx {
    /// Returns a bounded observation without calldata, raw victim, or signed bytes.
    pub fn observation(&self) -> UnsignedTxShapeObservation {
        debug_assert_eq!(self.unsigned_tx.signature_hash(), self.observation.unsigned_signing_hash);
        let _guard = &self.observation_guard;
        self.observation.clone()
    }

    /// Returns the installed identity bound to the observation.
    pub const fn execution(&self) -> &InstalledExecutionIdentity {
        &self.execution
    }

    /// Returns the actual evaluated V2 economics terminal retained by production finalization.
    pub const fn priority_economics(&self) -> &PriorityEconomicsV2 {
        &self.priority_economics
    }

    /// Revalidates the exact captured snapshot immediately before observation drain.
    pub fn validate_at_drain(&self) -> Result<(), TxAuthorityError> {
        if self.snapshot_freshness.is_current().unwrap_or(false) {
            Ok(())
        } else {
            Err(TxAuthorityError::SnapshotStaleAtDrain)
        }
    }

    /// Borrows the raw unsigned transaction under the bridge-issued access capability.
    #[cfg(feature = "t4e-handoff")]
    pub(crate) const fn unsigned_tx_with_bridge_access(
        &self,
        _access: &BridgeConversionSeal,
    ) -> &TxEip1559 {
        &self.unsigned_tx
    }

    #[cfg(feature = "t4e-handoff")]
    pub(crate) const fn amount(&self) -> U256 {
        self.amount
    }

    #[cfg(feature = "t4e-handoff")]
    pub(crate) const fn economics(&self) -> PriorityEconomicsReceipt {
        self.economics
    }
}

/// Node-local Base authority and atomic observation-cardinality owner.
#[derive(Debug)]
pub struct TxAuthorityAssembler {
    node: Arc<dyn TxAuthorityNodeView>,
    executor: DeployedContractIdentity,
    sender: Address,
    adapters: ProtocolAdapterMapping,
    held: Arc<AtomicBool>,
}

impl TxAuthorityAssembler {
    /// Installs the exact Base pins after validating the current committed parent.
    pub fn base_mainnet(node: Arc<dyn TxAuthorityNodeView>) -> Result<Self, TxAuthorityError> {
        Self::install(
            node,
            DeployedContractIdentity {
                address: EXECUTOR_ADDRESS,
                runtime_hash: EXECUTOR_RUNTIME_HASH,
            },
            EXECUTOR_SENDER,
            ProtocolAdapterMapping::base_mainnet_pins(),
        )
    }

    fn install(
        node: Arc<dyn TxAuthorityNodeView>,
        executor: DeployedContractIdentity,
        sender: Address,
        adapters: ProtocolAdapterMapping,
    ) -> Result<Self, TxAuthorityError> {
        if node.chain_id().map_err(|_| TxAuthorityError::DeploymentIdentityRejected)?
            != CHAIN_ID_BASE
        {
            return Err(TxAuthorityError::DeploymentIdentityRejected);
        }
        let parent =
            node.current_parent_hash().map_err(|_| TxAuthorityError::DeploymentIdentityRejected)?;
        let state = node
            .read_state_at_parent(parent, sender, Self::contract_addresses(&executor, &adapters))
            .map_err(|_| TxAuthorityError::DeploymentIdentityRejected)?;
        Self::validate_state_codes(&state, parent, &executor, &adapters)?;
        if node.current_parent_hash().map_err(|_| TxAuthorityError::DeploymentIdentityRejected)?
            != parent
        {
            return Err(TxAuthorityError::DeploymentIdentityRejected);
        }
        Ok(Self { node, executor, sender, adapters, held: Arc::new(AtomicBool::new(false)) })
    }

    /// Prepares a linear unsigned candidate without consulting economics authority.
    pub fn prepare_pre_economics(
        &self,
        candidate: &CandidateAssemblyView<'_>,
    ) -> Result<PreEconomicsCandidate, TxAuthorityError> {
        let view = AuthorityAssemblyView::from_candidate(candidate);
        Self::checkpoint(view.probe())?;
        let snapshot = view.snapshot();
        let plan = view.plan();
        let frame = view.frame;
        Self::validate_frame(snapshot, plan, frame)?;
        let (victim_priority, base_fee, max_fee) = Self::derive_fees(&view, frame)?;
        if plan.route[0].token_in != WETH
            || plan.route[0].token_out != plan.route[1].token_in
            || plan.route[1].token_out != WETH
        {
            return Err(TxAuthorityError::PriorityEconomicsRejected);
        }
        let floors = Self::requote(&view)?;
        Self::checkpoint(view.probe())?;
        let (execution, committed_nonce) = self.validate_execution(snapshot, frame.parent_hash)?;
        let valid_until_block = frame
            .block_number
            .checked_add(T4B_VALID_WINDOW_BLOCKS)
            .ok_or(TxAuthorityError::AssemblyRejected)?;
        let witness = self.capture_nonce(snapshot, &execution, frame, committed_nonce)?;
        let snapshot_freshness = self
            .node
            .capture_snapshot_freshness(snapshot)
            .map_err(|_| TxAuthorityError::NonceWitnessUnavailable)?;
        let observation_guard = self.acquire_guard()?;
        let calldata = self.encode_calldata(plan, floors, &execution, valid_until_block)?;
        let unsigned_tx = TxEip1559 {
            chain_id: CHAIN_ID_BASE,
            nonce: witness.shape_nonce,
            gas_limit: T4B_EXECUTOR_GAS_LIMIT,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: victim_priority,
            to: TxKind::Call(execution.executor.address),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::from(calldata),
        };
        Self::checkpoint(view.probe())?;
        let unsigned_signing_hash = unsigned_tx.signature_hash();
        let hop_protocols = [plan.route[0].protocol, plan.route[1].protocol];
        let hop_adapters =
            hop_protocols.map(|protocol| execution.adapters.resolve(protocol).address);
        let hop_runtime_hashes =
            hop_protocols.map(|protocol| execution.adapters.resolve(protocol).runtime_hash);
        let observation = UnsignedTxShapeObservation {
            frame,
            victim: plan.victim,
            plan_digest: plan.digest.0,
            sender: execution.sender,
            nonce: witness.shape_nonce,
            chain_id: CHAIN_ID_BASE,
            executor: execution.executor.address,
            hop_protocols,
            hop_adapters,
            hop_runtime_hashes,
            gas_limit: T4B_EXECUTOR_GAS_LIMIT,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: victim_priority,
            base_fee,
            valid_until_block,
            unsigned_signing_hash,
        };
        let parent_header = snapshot.latest_header();
        let observed_base_fee =
            u64::try_from(base_fee).map_err(|_| TxAuthorityError::FeeAuthorityRejected)?;
        if parent_header.parent_hash != frame.parent_hash
            || parent_header.number != frame.block_number
            || parent_header.base_fee_per_gas != Some(observed_base_fee)
        {
            return Err(TxAuthorityError::PlanOrFrameRejected);
        }
        let domain_digest = |domain: u8| {
            let mut bytes = Vec::with_capacity(65);
            bytes.push(domain);
            bytes.extend_from_slice(frame.parent_hash.as_slice());
            bytes.extend_from_slice(plan.digest.0.as_slice());
            keccak256(bytes)
        };
        let header_identity_digest = {
            let mut bytes = Vec::with_capacity(145);
            bytes.extend_from_slice(parent_header.hash().as_slice());
            bytes.extend_from_slice(parent_header.parent_hash.as_slice());
            bytes.extend_from_slice(&parent_header.number.to_be_bytes());
            bytes.extend_from_slice(&parent_header.timestamp.to_be_bytes());
            bytes.extend_from_slice(&parent_header.gas_limit.to_be_bytes());
            bytes.extend_from_slice(&observed_base_fee.to_be_bytes());
            bytes.extend_from_slice(parent_header.mix_hash.as_slice());
            keccak256(bytes)
        };
        let route_adapters = [
            execution.adapters.resolve(hop_protocols[0]).clone(),
            execution.adapters.resolve(hop_protocols[1]).clone(),
        ];
        let bindings = CheckedBindings {
            frame,
            header_identity_digest,
            beryl_env: CheckedBerylEnvInputs {
                chain_id: CHAIN_ID_BASE,
                block_number: parent_header.number,
                timestamp: parent_header.timestamp,
                gas_limit: parent_header.gas_limit,
                base_fee_per_gas: parent_header
                    .base_fee_per_gas
                    .ok_or(TxAuthorityError::FeeAuthorityRejected)?,
                prev_randao: parent_header.mix_hash,
                excess_blob_gas: parent_header.excess_blob_gas,
            },
            sender: execution.sender,
            kickback_recipient: address!("743be0db30148336a3db479f19d4e1828b293869"),
            route_hops: plan.route.clone(),
            route_pools: [plan.route[0].pool, plan.route[1].pool],
            route_tokens: [
                plan.route[0].token_in,
                plan.route[0].token_out,
                plan.route[1].token_out,
            ],
            header_coinbase: parent_header.beneficiary,
            deployment: DeploymentWitness {
                validated_parent: execution.validated_parent,
                executor: execution.executor.clone(),
                route_adapters,
            },
            nonce: NonceWitness {
                sender: witness.sender,
                parent_hash: witness.parent_hash,
                committed_nonce: witness.committed_nonce,
                pending_overlay_nonce: witness.pending_overlay_nonce,
                shape_nonce: witness.shape_nonce,
            },
            freshness: FreshnessWitness {
                parent_hash: frame.parent_hash,
                snapshot_parent_hash: snapshot.parent_hash(),
                valid_until_block,
                snapshot_identity_digest: domain_digest(2),
            },
            frame_digest: domain_digest(3),
            plan_digest: plan.digest.0,
            route_digest: domain_digest(4),
            shape_digest: domain_digest(5),
            overlay_digest: domain_digest(6),
            order_digest: domain_digest(7),
            state_digest: domain_digest(8),
            access_digest: domain_digest(9),
            unsigned_signing_hash,
            parent_header,
        };
        Ok(PreEconomicsCandidate {
            unsigned_tx,
            amount: plan.amount_in,
            gross_profit: plan.gross_profit,
            victim_priority,
            victim_max_fee: max_fee,
            bindings,
            observation,
            execution,
            observation_guard,
            snapshot_freshness,
            node: Arc::clone(&self.node),
        })
    }

    /// Consumes execution evidence, rechecks freshness and bindings, and seals the candidate.
    pub fn finalize(
        &self,
        ready: EconomicsReadyCandidate,
        ledger: &PriorityEconomicsLedgerV2,
    ) -> Result<ValidatedUnsignedAtomicTx, TxAuthorityError> {
        let EconomicsReadyCandidate { pre, evidence } = ready;
        let bindings = &pre.bindings;
        if !Arc::ptr_eq(&self.node, &pre.node)
            || pre.node.current_parent_hash().ok() != Some(bindings.frame.parent_hash)
            || !pre.snapshot_freshness.is_current().unwrap_or(false)
            || evidence.frame_digest != bindings.frame_digest
            || evidence.plan_digest != bindings.plan_digest
            || evidence.route_digest != bindings.route_digest
            || evidence.shape_digest != bindings.shape_digest
            || evidence.overlay_digest != bindings.overlay_digest
            || evidence.order_digest != bindings.order_digest
            || evidence.state_digest != bindings.state_digest
            || evidence.access_digest != bindings.access_digest
            || evidence.unsigned_signing_hash != bindings.unsigned_signing_hash
            || evidence.deployment_parent != bindings.deployment.validated_parent
            || evidence.nonce_parent != bindings.nonce.parent_hash
            || evidence.freshness_parent != bindings.freshness.parent_hash
            || evidence.snapshot_identity_digest != bindings.freshness.snapshot_identity_digest
            || pre.unsigned_tx.signature_hash() != bindings.unsigned_signing_hash
        {
            return Err(TxAuthorityError::NonceWitnessStaleBeforePublish);
        }
        let state = pre
            .node
            .read_state_at_parent(
                bindings.frame.parent_hash,
                bindings.sender,
                Self::contract_addresses(&self.executor, &self.adapters),
            )
            .map_err(|_| TxAuthorityError::NonceWitnessStaleBeforePublish)?;
        Self::validate_state_codes(
            &state,
            bindings.frame.parent_hash,
            &self.executor,
            &self.adapters,
        )
        .map_err(|_| TxAuthorityError::NonceWitnessStaleBeforePublish)?;
        if state.committed_sender_nonce.unwrap_or(0) != bindings.nonce.committed_nonce {
            return Err(TxAuthorityError::NonceWitnessStaleBeforePublish);
        }
        let decision = evaluate(PriorityFilterInput {
            gross_profit_wei: Some(pre.gross_profit),
            authority: Some(evidence.authority),
            victim_max_priority_fee_per_gas_wei: Some(U256::from(pre.victim_priority)),
            victim_max_fee_per_gas_wei: Some(U256::from(pre.victim_max_fee)),
            candidate_block: bindings.frame.block_number,
        })
        .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?;
        if evidence.weth_delta != decision.kickback_wei() {
            return Err(TxAuthorityError::PriorityEconomicsRejected);
        }
        let priority_economics = Self::evaluated_terminal(&pre, &evidence, &decision)?;
        ledger
            .append(priority_economics.clone())
            .map_err(|_| TxAuthorityError::PriorityEconomicsLedgerUnavailable)?;
        Ok(ValidatedUnsignedAtomicTx {
            unsigned_tx: pre.unsigned_tx,
            #[cfg(feature = "t4e-handoff")]
            amount: pre.amount,
            #[cfg(feature = "t4e-handoff")]
            economics: decision,
            priority_economics,
            observation: pre.observation,
            execution: pre.execution,
            observation_guard: pre.observation_guard,
            snapshot_freshness: pre.snapshot_freshness,
        })
    }

    fn evaluated_terminal(
        pre: &PreEconomicsCandidate,
        evidence: &CandidateEconomicsEvidence,
        decision: &crate::PriorityEconomicsReceipt,
    ) -> Result<PriorityEconomicsV2, TxAuthorityError> {
        let bindings = &pre.bindings;
        let adapters = bindings.deployment.route_adapters();
        let hops = &bindings.route_hops;
        let canonical = evidence.canonical_l1.selected();
        let route = SelectedRouteEvidenceV2::new(
            bindings.frame.victim,
            bindings.route_digest,
            bindings.route_pools,
            bindings.route_tokens,
            [adapters[0].runtime_hash(), adapters[1].runtime_hash()],
            [hops[0].fee_pips, hops[1].fee_pips],
            [
                hops[0].token_in.as_slice() < hops[0].token_out.as_slice(),
                hops[1].token_in.as_slice() < hops[1].token_out.as_slice(),
            ],
            pre.amount,
            bindings.frame_digest,
            bindings.header_identity_digest,
            bindings.state_digest,
            bindings.plan_digest,
            bindings.shape_digest,
            canonical.digest(),
        )
        .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?;
        let l1_fee = CanonicalL1FeeEvidenceV2::new(
            u64::try_from(canonical.encoded_length())
                .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?,
            u64::try_from(canonical.zero_bytes())
                .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?,
            u64::try_from(canonical.non_zero_bytes())
                .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?,
            u64::try_from(canonical.fast_lz_size())
                .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?,
            canonical.digest(),
            evidence.canonical_l1.fee(),
        )
        .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?;
        let amount_out = pre
            .amount
            .checked_add(pre.gross_profit)
            .ok_or(TxAuthorityError::PriorityEconomicsRejected)?;
        let actual_gas_used = u64::try_from(decision.execution_gas_estimate())
            .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?;
        let counters = PriorityEconomicsCountersV2::new(1, 1, 1, 1, 1, 1, 1, 0)
            .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)?;
        PriorityEconomicsV2::evaluated_from_execution(
            route,
            amount_out,
            evidence.weth_delta,
            decision.retained_value_wei(),
            actual_gas_used,
            decision.l2_execution_fee_wei(),
            l1_fee,
            decision.admitted().then_some(true),
            counters,
        )
        .map_err(|_| TxAuthorityError::PriorityEconomicsRejected)
    }

    fn checkpoint(probe: &base_mev_trader::CancellationProbe) -> Result<(), TxAuthorityError> {
        let now = Instant::now();
        if probe.checkpoint(now, true) {
            return Ok(());
        }
        probe.acknowledge_drop();
        if now >= probe.token().deadline() {
            Err(TxAuthorityError::DeadlineNoShape)
        } else {
            Err(TxAuthorityError::Cancelled)
        }
    }

    fn validate_frame(
        snapshot: &SnapshotHandle,
        plan: &base_mev_trader::BackrunPlan,
        frame: MeasurementContext,
    ) -> Result<(), TxAuthorityError> {
        MeasurementEncoder::validate(plan).map_err(|_| TxAuthorityError::PlanOrFrameRejected)?;
        if plan.parent_hash != frame.parent_hash
            || plan.block_number != frame.block_number
            || plan.predecessor_index != frame.predecessor_index
            || plan.payload_id != frame.payload_id
            || plan.victim != frame.victim
            || snapshot.parent_hash() != frame.parent_hash
            || snapshot.latest_block_number() != frame.block_number
        {
            return Err(TxAuthorityError::PlanOrFrameRejected);
        }
        Ok(())
    }

    fn derive_fees(
        view: &AuthorityAssemblyView<'_>,
        frame: MeasurementContext,
    ) -> Result<(u128, u128, u128), TxAuthorityError> {
        let raw: &[u8] = view.victim_raw().as_ref();
        if raw.first() != Some(&0x02) || keccak256(raw) != frame.victim {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        }
        let mut bytes = raw;
        let envelope = TxEnvelope::decode_2718(&mut bytes)
            .map_err(|_| TxAuthorityError::FeeAuthorityRejected)?;
        if !bytes.is_empty() {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        }
        let TxEnvelope::Eip1559(signed) = envelope else {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        };
        let victim = signed.tx();
        if victim.chain_id != CHAIN_ID_BASE {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        }
        let header = view.snapshot().latest_header();
        if header.number != frame.block_number {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        }
        let base_fee = header
            .base_fee_per_gas
            .map(u128::from)
            .filter(|fee| *fee != 0)
            .ok_or(TxAuthorityError::FeeAuthorityRejected)?;
        let priority = victim.max_priority_fee_per_gas;
        if priority == 0 {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        }
        let max_fee =
            base_fee.checked_add(priority).ok_or(TxAuthorityError::FeeAuthorityRejected)?;
        if victim.max_fee_per_gas < max_fee {
            return Err(TxAuthorityError::FeeAuthorityRejected);
        }
        Ok((priority, base_fee, max_fee))
    }

    fn requote(view: &AuthorityAssemblyView<'_>) -> Result<[U256; 2], TxAuthorityError> {
        let plan = view.plan();
        let first = Self::unique_pool(view.prepared(), &plan.route[0])?;
        let second = Self::unique_pool(view.prepared(), &plan.route[1])?;
        Self::checkpoint(view.probe())?;
        let first_out =
            match first.quote_exact_in(plan.route[0].token_in, plan.amount_in, view.probe()) {
                Ok(output) => output,
                Err(_) => {
                    return Err(Self::checkpoint(view.probe())
                        .err()
                        .unwrap_or(TxAuthorityError::RequoteRejected));
                }
            };
        let second_out =
            match second.quote_exact_in(plan.route[1].token_in, first_out, view.probe()) {
                Ok(output) => output,
                Err(_) => {
                    return Err(Self::checkpoint(view.probe())
                        .err()
                        .unwrap_or(TxAuthorityError::RequoteRejected));
                }
            };
        if first_out.is_zero() || second_out.is_zero() || second_out != plan.amount_out {
            return Err(TxAuthorityError::RequoteRejected);
        }
        Ok([first_out, second_out])
    }

    fn unique_pool<'a>(
        prepared: &'a [PreparedPoolState],
        hop: &BackrunHop,
    ) -> Result<&'a PreparedPoolState, TxAuthorityError> {
        let mut matches = prepared.iter().filter(|pool| {
            pool.pool == hop.pool
                && pool.protocol == hop.protocol
                && pool.fee_pips == hop.fee_pips
                && ((pool.token0 == hop.token_in && pool.token1 == hop.token_out)
                    || (pool.token1 == hop.token_in && pool.token0 == hop.token_out))
        });
        let found = matches.next().ok_or(TxAuthorityError::RequoteRejected)?;
        if matches.next().is_some() {
            return Err(TxAuthorityError::RequoteRejected);
        }
        Ok(found)
    }

    fn validate_execution(
        &self,
        snapshot: &SnapshotHandle,
        parent: B256,
    ) -> Result<(InstalledExecutionIdentity, u64), TxAuthorityError> {
        if !self.node.is_current_authoritative(snapshot).unwrap_or(false)
            || self.node.chain_id().ok() != Some(CHAIN_ID_BASE)
            || self.node.current_parent_hash().ok() != Some(parent)
        {
            return Err(TxAuthorityError::DeploymentIdentityRejected);
        }
        let executor = self.executor.clone();
        let state = self
            .node
            .read_state_at_parent(
                parent,
                self.sender,
                Self::contract_addresses(&executor, &self.adapters),
            )
            .map_err(|_| TxAuthorityError::DeploymentIdentityRejected)?;
        Self::validate_state_codes(&state, parent, &executor, &self.adapters)?;
        if !self.node.is_current_authoritative(snapshot).unwrap_or(false)
            || self.node.current_parent_hash().ok() != Some(parent)
        {
            return Err(TxAuthorityError::DeploymentIdentityRejected);
        }
        Ok((
            InstalledExecutionIdentity {
                executor,
                adapters: self.adapters.clone(),
                sender: self.sender,
                validated_parent: parent,
            },
            state.committed_sender_nonce.unwrap_or(0),
        ))
    }

    const fn contract_addresses(
        executor: &DeployedContractIdentity,
        adapters: &ProtocolAdapterMapping,
    ) -> [Address; 4] {
        [
            executor.address,
            adapters.uniswap_v2.address,
            adapters.uniswap_v3.address,
            adapters.aerodrome.address,
        ]
    }

    fn validate_state_codes(
        state: &TxAuthorityStateRead,
        parent: B256,
        executor: &DeployedContractIdentity,
        adapters: &ProtocolAdapterMapping,
    ) -> Result<(), TxAuthorityError> {
        if state.parent_hash != parent {
            return Err(TxAuthorityError::DeploymentIdentityRejected);
        }
        let identities = [
            executor.clone(),
            adapters.uniswap_v2.clone(),
            adapters.uniswap_v3.clone(),
            adapters.aerodrome.clone(),
        ];
        for (code, identity) in state.runtime_codes.iter().zip(identities) {
            let code = code
                .as_ref()
                .filter(|code| !code.is_empty())
                .ok_or(TxAuthorityError::DeploymentIdentityRejected)?;
            if keccak256(code) != identity.runtime_hash {
                return Err(TxAuthorityError::DeploymentIdentityRejected);
            }
        }
        Ok(())
    }

    fn capture_nonce(
        &self,
        snapshot: &SnapshotHandle,
        execution: &InstalledExecutionIdentity,
        frame: MeasurementContext,
        committed_nonce: u64,
    ) -> Result<SnapshotNonceWitness, TxAuthorityError> {
        if execution.validated_parent != frame.parent_hash
            || snapshot.parent_hash() != frame.parent_hash
            || !self.node.is_current_authoritative(snapshot).unwrap_or(false)
        {
            return Err(TxAuthorityError::NonceWitnessUnavailable);
        }
        let overlay = snapshot
            .pending_account_nonce(execution.sender)
            .map_err(|_| TxAuthorityError::NonceWitnessUnavailable)?;
        let (pending_overlay_nonce, shape_nonce) = if let Some(pending) = overlay {
            if pending.original_nonce() != committed_nonce
                || pending.current_nonce() < pending.original_nonce()
            {
                return Err(TxAuthorityError::NonceWitnessUnavailable);
            }
            (Some(pending.current_nonce()), pending.current_nonce())
        } else {
            (None, committed_nonce)
        };
        if !self.node.is_current_authoritative(snapshot).unwrap_or(false) {
            return Err(TxAuthorityError::NonceWitnessUnavailable);
        }
        Ok(SnapshotNonceWitness {
            sender: execution.sender,
            parent_hash: frame.parent_hash,
            committed_nonce,
            pending_overlay_nonce,
            shape_nonce,
        })
    }

    fn acquire_guard(&self) -> Result<MeasurementNonceGuard, TxAuthorityError> {
        self.held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| TxAuthorityError::ObservationBusy)?;
        Ok(MeasurementNonceGuard { held: Arc::clone(&self.held) })
    }

    fn encode_calldata(
        &self,
        plan: &base_mev_trader::BackrunPlan,
        floors: [U256; 2],
        execution: &InstalledExecutionIdentity,
        valid_until_block: u64,
    ) -> Result<Vec<u8>, TxAuthorityError> {
        let make_hop = |index: usize| -> Result<ValidatedAbiHop, TxAuthorityError> {
            let hop = &plan.route[index];
            let adapter = execution.adapters.resolve(hop.protocol).address;
            let funding_target = match hop.protocol {
                ExactProtocol::UniswapV2
                | ExactProtocol::AerodromeVolatile
                | ExactProtocol::AerodromeStable => hop.pool,
                ExactProtocol::UniswapV3 => adapter,
            };
            let fee_bps = fee_bps_for_executor(hop.protocol, hop.fee_pips)
                .map_err(|_| TxAuthorityError::AssemblyRejected)?;
            Ok(ValidatedAbiHop {
                adapter,
                pool: hop.pool,
                token_in: hop.token_in,
                token_out: hop.token_out,
                fee_bps,
                min_amount_out: floors[index],
                funding_target,
            })
        };
        let call = ValidatedAtomicCall {
            hops: [make_hop(0)?, make_hop(1)?],
            amount_in: plan.amount_in,
            min_final_amount: plan.amount_out,
            valid_until_block,
        };
        Ok(AtomicCalldataEncoder::encode_validated(&call))
    }
}
