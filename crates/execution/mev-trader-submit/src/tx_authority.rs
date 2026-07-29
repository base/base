//! Node-local authority derivation for bounded, unsigned T4b transaction-shape observation.

use std::{
    fmt::{self, Debug},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::{eip2718::Decodable2718, eip2930::AccessList};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, b256, keccak256};
use base_mev_trader::{
    BackrunHop, CandidateAssemblyView, ExactProtocol, MeasurementContext, MeasurementEncoder,
    PreparedPoolState, SnapshotHandle,
};

use crate::{calldata::AtomicCalldataEncoder, fee::fee_bps_for_executor};
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
    const fn from_candidate(view: CandidateAssemblyView<'a>) -> Self {
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

    /// Derives and validates one linear unsigned shape from same-frame borrowed artifacts.
    pub fn assemble_validated(
        &self,
        view: CandidateAssemblyView<'_>,
    ) -> Result<ValidatedUnsignedAtomicTx, TxAuthorityError> {
        self.assemble_view(AuthorityAssemblyView::from_candidate(view))
    }

    fn assemble_view(
        &self,
        view: AuthorityAssemblyView<'_>,
    ) -> Result<ValidatedUnsignedAtomicTx, TxAuthorityError> {
        Self::checkpoint(view.probe())?;
        let snapshot = view.snapshot();
        let plan = view.plan();
        let frame = view.frame;
        Self::validate_frame(snapshot, plan, frame)?;
        let (victim_priority, base_fee, max_fee) = Self::derive_fees(&view, frame)?;
        let floors = Self::requote(&view)?;
        Self::checkpoint(view.probe())?;
        let (execution, committed_nonce) = self.validate_execution(snapshot, frame.parent_hash)?;
        Self::checkpoint(view.probe())?;
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
        if unsigned_tx.chain_id != CHAIN_ID_BASE
            || unsigned_tx.nonce != witness.shape_nonce
            || unsigned_tx.gas_limit != T4B_EXECUTOR_GAS_LIMIT
            || unsigned_tx.max_fee_per_gas != max_fee
            || unsigned_tx.max_priority_fee_per_gas != victim_priority
            || unsigned_tx.to != TxKind::Call(execution.executor.address)
            || !unsigned_tx.value.is_zero()
            || !unsigned_tx.access_list.is_empty()
        {
            return Err(TxAuthorityError::AssemblyRejected);
        }
        Self::checkpoint(view.probe())?;
        let (fresh_execution, fresh_committed_nonce) = self
            .validate_execution(snapshot, frame.parent_hash)
            .map_err(|_| TxAuthorityError::NonceWitnessStaleBeforePublish)?;
        let fresh = self
            .capture_nonce(snapshot, &fresh_execution, frame, fresh_committed_nonce)
            .map_err(|_| TxAuthorityError::NonceWitnessStaleBeforePublish)?;
        if fresh_execution != execution
            || fresh != witness
            || !snapshot_freshness.is_current().unwrap_or(false)
        {
            return Err(TxAuthorityError::NonceWitnessStaleBeforePublish);
        }
        Self::checkpoint(view.probe())?;

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
            unsigned_signing_hash: unsigned_tx.signature_hash(),
        };
        Self::checkpoint(view.probe())?;
        Ok(ValidatedUnsignedAtomicTx {
            unsigned_tx,
            #[cfg(feature = "t4e-handoff")]
            amount: plan.amount_in,
            observation,
            execution,
            observation_guard,
            snapshot_freshness,
        })
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "t4d-bridge")]
    use std::collections::BTreeSet;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    use alloy_consensus::{Header, Sealed, SignableTransaction};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, Signature, U256, address, b256, keccak256};
    use base_mev_trader::{
        BackrunHop, BundleVisitor, CancellationProbe, CancellationToken, ExactProtocol,
        GlobalLifecycle, MeasurementEncoder, PayloadVisitor, PendingAccountNonce,
        PendingSnapshotView, PortError, PreparedPoolQuote, PreparedPoolState,
        SnapshotCaptureCoordinator, SnapshotHandleFactory, TraderSnapshotPort, TransactionVisitor,
        VisitSummary,
    };
    use reth_provider::StateProviderBox;

    use super::*;
    #[cfg(feature = "t4e-handoff")]
    use crate::arm::{CodeHashProvider, ProviderError};

    #[derive(Debug)]
    struct TestFreshness(Arc<AtomicBool>);

    impl SnapshotFreshnessToken for TestFreshness {
        fn is_current(&self) -> Result<bool, TxAuthorityNodeError> {
            Ok(self.0.load(Ordering::Acquire))
        }
    }
    #[cfg(feature = "t4e-handoff")]
    #[derive(Debug)]
    struct TestBlockProvider {
        current_block: Arc<AtomicU64>,
        unavailable: Arc<AtomicBool>,
    }

    #[cfg(feature = "t4e-handoff")]
    impl CodeHashProvider for TestBlockProvider {
        fn code_hash_at_latest_committed(&self, _addr: Address) -> Result<B256, ProviderError> {
            panic!("deadline revalidation must not read code hash")
        }

        fn current_block(&self) -> Result<u64, ProviderError> {
            if self.unavailable.load(Ordering::Acquire) {
                Err(ProviderError::Unavailable("committed head unavailable".to_string()))
            } else {
                Ok(self.current_block.load(Ordering::Acquire))
            }
        }
    }

    #[derive(Debug)]
    struct TestSnapshotView {
        parent_hash: B256,
        block_number: u64,
        base_fee: Option<u64>,
        pending_nonce: Option<PendingAccountNonce>,
    }

    impl PendingSnapshotView for TestSnapshotView {
        fn parent_hash(&self) -> B256 {
            self.parent_hash
        }

        fn latest_block_number(&self) -> u64 {
            self.block_number
        }

        fn canonical_block_number(&self) -> u64 {
            self.block_number.saturating_sub(1)
        }

        fn latest_flashblock_index(&self) -> u64 {
            1
        }

        fn latest_header(&self) -> Sealed<Header> {
            Sealed::new_unchecked(
                Header {
                    number: self.block_number,
                    base_fee_per_gas: self.base_fee,
                    ..Header::default()
                },
                B256::with_last_byte(99),
            )
        }

        fn pending_account_nonce(
            &self,
            _address: Address,
        ) -> Result<Option<PendingAccountNonce>, PortError> {
            Ok(self.pending_nonce)
        }

        fn latest_block_transaction_count(&self) -> usize {
            1
        }

        fn has_transaction_hash(&self, _transaction_hash: B256) -> bool {
            true
        }

        fn transaction_position(
            &self,
            _block_number: u64,
            _transaction_hash: B256,
        ) -> Option<usize> {
            Some(0)
        }

        fn visit_latest_block_payloads(
            &self,
            _visitor: &mut dyn PayloadVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }

        fn visit_transactions_for_block(
            &self,
            _block_number: u64,
            _start: usize,
            _limit: usize,
            _visitor: &mut dyn TransactionVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }

        fn visit_bundle(
            &self,
            _visitor: &mut dyn BundleVisitor,
        ) -> Result<VisitSummary, PortError> {
            Ok(VisitSummary { visited: 0, complete: true })
        }
    }

    #[derive(Debug)]
    struct TestSnapshotPort {
        view: Arc<dyn PendingSnapshotView + Send + Sync>,
        received_at: Instant,
        current: Arc<AtomicBool>,
    }

    impl TraderSnapshotPort for TestSnapshotPort {
        fn capture_latest(
            &self,
            factory: &SnapshotHandleFactory,
        ) -> Result<Option<SnapshotHandle>, PortError> {
            factory.issue(Arc::clone(&self.view), self.received_at).map(Some)
        }

        fn is_current_authoritative(&self, handle: &SnapshotHandle) -> bool {
            self.current.load(Ordering::Acquire)
                && handle.matches_capture(&self.view, self.received_at)
        }

        fn state_at_hash(&self, _block_hash: B256) -> Result<StateProviderBox, PortError> {
            Err(PortError::ProviderUnavailable)
        }

        fn sealed_header_at_hash(&self, _block_hash: B256) -> Result<Sealed<Header>, PortError> {
            Err(PortError::HeaderUnavailable)
        }
    }

    #[derive(Debug)]
    struct NoopNode;

    impl TxAuthorityNodeView for NoopNode {
        fn chain_id(&self) -> Result<u64, TxAuthorityNodeError> {
            Ok(CHAIN_ID_BASE)
        }

        fn current_parent_hash(&self) -> Result<B256, TxAuthorityNodeError> {
            Ok(B256::ZERO)
        }

        fn read_state_at_parent(
            &self,
            parent_hash: B256,
            _sender: Address,
            _contracts: [Address; 4],
        ) -> Result<TxAuthorityStateRead, TxAuthorityNodeError> {
            Ok(TxAuthorityStateRead::new(
                parent_hash,
                Some(0),
                std::array::from_fn(|_| Some(Bytes::from_static(b"code"))),
            ))
        }

        fn is_current_authoritative(
            &self,
            _snapshot: &SnapshotHandle,
        ) -> Result<bool, TxAuthorityNodeError> {
            Ok(true)
        }

        fn capture_snapshot_freshness(
            &self,
            _snapshot: &SnapshotHandle,
        ) -> Result<Box<dyn SnapshotFreshnessToken>, TxAuthorityNodeError> {
            Ok(Box::new(TestFreshness(Arc::new(AtomicBool::new(true)))))
        }
    }

    #[derive(Debug)]
    struct TestNode {
        parent_hash: B256,
        sender: Address,
        contracts: [Address; 4],
        state: TxAuthorityStateRead,
        current: Arc<AtomicBool>,
        reads: AtomicU64,
        stale_on_read: Option<u64>,
        delay_on_read: Option<(u64, Duration)>,
        state_error: Arc<AtomicBool>,
        head_flip_after_read: Arc<AtomicBool>,
        stale_code_index: Arc<AtomicU64>,
    }

    impl TxAuthorityNodeView for TestNode {
        fn chain_id(&self) -> Result<u64, TxAuthorityNodeError> {
            Ok(CHAIN_ID_BASE)
        }

        fn current_parent_hash(&self) -> Result<B256, TxAuthorityNodeError> {
            if self.head_flip_after_read.load(Ordering::Acquire)
                && self.reads.load(Ordering::SeqCst) > 0
            {
                return Ok(B256::with_last_byte(self.parent_hash.as_slice()[31].wrapping_add(1)));
            }
            Ok(self.parent_hash)
        }

        fn read_state_at_parent(
            &self,
            parent_hash: B256,
            sender: Address,
            contracts: [Address; 4],
        ) -> Result<TxAuthorityStateRead, TxAuthorityNodeError> {
            if self.state_error.load(Ordering::Acquire) {
                return Err(TxAuthorityNodeError::Unavailable);
            }
            if parent_hash != self.parent_hash
                || sender != self.sender
                || contracts != self.contracts
            {
                return Err(TxAuthorityNodeError::Incoherent);
            }
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            if self.delay_on_read.is_some_and(|(threshold, _)| read >= threshold) {
                std::thread::sleep(self.delay_on_read.expect("checked delay").1);
            }
            let mut state = self.state.clone();
            if self.stale_on_read.is_some_and(|threshold| read >= threshold) {
                state.committed_sender_nonce =
                    state.committed_sender_nonce.and_then(|nonce| nonce.checked_add(1));
            }
            let stale_code_index = self.stale_code_index.load(Ordering::Acquire) as usize;
            if let Some(code) = state.runtime_codes.get_mut(stale_code_index) {
                *code = Some(Bytes::from_static(b"wrong-runtime"));
            }
            Ok(state)
        }

        fn is_current_authoritative(
            &self,
            _snapshot: &SnapshotHandle,
        ) -> Result<bool, TxAuthorityNodeError> {
            Ok(self.current.load(Ordering::Acquire))
        }

        fn capture_snapshot_freshness(
            &self,
            _snapshot: &SnapshotHandle,
        ) -> Result<Box<dyn SnapshotFreshnessToken>, TxAuthorityNodeError> {
            Ok(Box::new(TestFreshness(Arc::clone(&self.current))))
        }
    }

    #[derive(Debug)]
    struct AssemblyFixture {
        assembler: TxAuthorityAssembler,
        #[cfg(feature = "t4d-bridge")]
        node: Arc<dyn TxAuthorityNodeView>,
        snapshot: SnapshotHandle,
        frame: MeasurementContext,
        prepared: Vec<PreparedPoolState>,
        plan: base_mev_trader::BackrunPlan,
        victim_raw: Bytes,
        probe: CancellationProbe,
        current: Arc<AtomicBool>,
        #[cfg(feature = "t4d-bridge")]
        state_error: Arc<AtomicBool>,
        #[cfg(feature = "t4d-bridge")]
        head_flip_after_read: Arc<AtomicBool>,
        #[cfg(feature = "t4e-handoff")]
        current_block: Arc<AtomicU64>,
        #[cfg(feature = "t4e-handoff")]
        block_unavailable: Arc<AtomicBool>,
        #[cfg(feature = "t4d-bridge")]
        stale_code_index: Arc<AtomicU64>,
    }

    impl AssemblyFixture {
        fn view(&self) -> AuthorityAssemblyView<'_> {
            AuthorityAssemblyView {
                snapshot: &self.snapshot,
                frame: self.frame,
                prepared: &self.prepared,
                plan: &self.plan,
                victim_raw: &self.victim_raw,
                probe: &self.probe,
            }
        }

        #[cfg(feature = "t4d-bridge")]
        fn bridge(&self) -> bridge::InstalledSubmissionBridge {
            bridge::InstalledSubmissionBridge::install_for_test(
                Arc::clone(&self.node),
                self.assembler.executor.clone(),
                self.assembler.sender,
                self.assembler.adapters.clone(),
            )
            .expect("same-provider bridge install")
        }

        #[cfg(feature = "t4e-handoff")]
        fn block_provider(&self) -> TestBlockProvider {
            TestBlockProvider {
                current_block: Arc::clone(&self.current_block),
                unavailable: Arc::clone(&self.block_unavailable),
            }
        }
    }
    fn revalidate_for_test<'a>(
        fixture: &AssemblyFixture,
        bridge: &bridge::InstalledSubmissionBridge,
        candidate: &'a bridge::SealedUnsignedCandidate,
    ) -> Result<&'a bridge::AdapterAwareProofBindings, bridge::BridgeError> {
        bridge.revalidate_for_handoff(
            candidate,
            #[cfg(feature = "t4e-handoff")]
            &fixture.block_provider(),
        )
    }

    fn assembly_fixture_with_read_delay(
        pending_nonce: Option<PendingAccountNonce>,
        stale_on_read: Option<u64>,
        delay_on_read: Option<(u64, Duration)>,
    ) -> AssemblyFixture {
        let parent_hash = B256::with_last_byte(21);
        let block_number = 22;
        let sender = Address::with_last_byte(23);
        let token_a = Address::with_last_byte(24);
        let token_b = Address::with_last_byte(25);
        let probe = probe();
        let prepared = vec![
            PreparedPoolState {
                pool: Address::with_last_byte(26),
                protocol: ExactProtocol::UniswapV2,
                token0: token_a,
                token1: token_b,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::constant_product(
                    U256::from(1_000_000u64),
                    U256::from(2_000_000u64),
                ),
            },
            PreparedPoolState {
                pool: Address::with_last_byte(27),
                protocol: ExactProtocol::UniswapV2,
                token0: token_a,
                token1: token_b,
                decimals0: 18,
                decimals1: 18,
                fee_pips: 3_000,
                quote: PreparedPoolQuote::constant_product(
                    U256::from(3_000_000u64),
                    U256::from(1_000_000u64),
                ),
            },
        ];
        let amount_in = U256::from(1_000u64);
        let first_out =
            prepared[0].quote_exact_in(token_a, amount_in, &probe).expect("first quote");
        let amount_out =
            prepared[1].quote_exact_in(token_b, first_out, &probe).expect("second quote");
        assert!(amount_out > amount_in);

        let victim = TxEip1559 {
            chain_id: CHAIN_ID_BASE,
            nonce: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 110,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::with_last_byte(28)),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
        }
        .into_signed(Signature::new(U256::from(1), U256::from(2), false));
        let victim_raw = Bytes::from(victim.encoded_2718());
        let victim_hash = keccak256(&victim_raw);

        let frame = MeasurementContext {
            parent_hash,
            block_number,
            predecessor_index: 1,
            payload_id: alloy_rpc_types_engine::PayloadId::new([4; 8]),
            victim: victim_hash,
        };
        let mut plan = base_mev_trader::BackrunPlan {
            parent_hash,
            block_number,
            predecessor_index: frame.predecessor_index,
            payload_id: frame.payload_id,
            victim: victim_hash,
            route: [
                BackrunHop {
                    pool: prepared[0].pool,
                    protocol: prepared[0].protocol,
                    token_in: token_a,
                    token_out: token_b,
                    fee_pips: prepared[0].fee_pips,
                },
                BackrunHop {
                    pool: prepared[1].pool,
                    protocol: prepared[1].protocol,
                    token_in: token_b,
                    token_out: token_a,
                    fee_pips: prepared[1].fee_pips,
                },
            ],
            amount_in,
            amount_out,
            gross_profit: amount_out - amount_in,
            digest: base_mev_trader::BackrunPlanDigest(B256::ZERO),
        };
        plan.digest = MeasurementEncoder::digest(&plan).expect("plan digest");

        let current = Arc::new(AtomicBool::new(true));
        let snapshot_view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(TestSnapshotView {
                parent_hash,
                block_number,
                base_fee: Some(100),
                pending_nonce,
            });
        let snapshot_port = TestSnapshotPort {
            view: snapshot_view,
            received_at: Instant::now(),
            current: Arc::clone(&current),
        };
        let snapshot = SnapshotCaptureCoordinator
            .capture(&snapshot_port)
            .expect("snapshot capture")
            .expect("current snapshot");

        let codes = [
            Bytes::from_static(b"executor"),
            Bytes::from_static(b"uniswap-v2"),
            Bytes::from_static(b"uniswap-v3"),
            Bytes::from_static(b"aerodrome"),
        ];
        let executor = DeployedContractIdentity {
            address: Address::with_last_byte(29),
            runtime_hash: keccak256(&codes[0]),
        };
        let adapters = ProtocolAdapterMapping {
            uniswap_v2: DeployedContractIdentity {
                address: Address::with_last_byte(30),
                runtime_hash: keccak256(&codes[1]),
            },
            uniswap_v3: DeployedContractIdentity {
                address: Address::with_last_byte(31),
                runtime_hash: keccak256(&codes[2]),
            },
            aerodrome: DeployedContractIdentity {
                address: Address::with_last_byte(32),
                runtime_hash: keccak256(&codes[3]),
            },
        };
        let contracts = TxAuthorityAssembler::contract_addresses(&executor, &adapters);
        #[cfg(feature = "t4e-handoff")]
        let current_block = Arc::new(AtomicU64::new(block_number - 1));
        #[cfg(feature = "t4e-handoff")]
        let block_unavailable = Arc::new(AtomicBool::new(false));
        let state = TxAuthorityStateRead::new(parent_hash, Some(4), codes.map(Some));
        let state_error = Arc::new(AtomicBool::new(false));
        let head_flip_after_read = Arc::new(AtomicBool::new(false));
        let stale_code_index = Arc::new(AtomicU64::new(u64::MAX));
        let node: Arc<dyn TxAuthorityNodeView> = Arc::new(TestNode {
            parent_hash,
            sender,
            contracts,
            state,
            current: Arc::clone(&current),
            reads: AtomicU64::new(0),
            stale_on_read,
            delay_on_read,
            state_error: Arc::clone(&state_error),
            head_flip_after_read: Arc::clone(&head_flip_after_read),
            stale_code_index: Arc::clone(&stale_code_index),
        });
        let assembler =
            TxAuthorityAssembler::install(Arc::clone(&node), executor, sender, adapters)
                .expect("same-parent authority install");

        AssemblyFixture {
            assembler,
            #[cfg(feature = "t4d-bridge")]
            node,
            snapshot,
            frame,
            prepared,
            plan,
            victim_raw,
            probe,
            current,
            #[cfg(feature = "t4d-bridge")]
            state_error,
            #[cfg(feature = "t4e-handoff")]
            current_block,
            #[cfg(feature = "t4e-handoff")]
            block_unavailable,
            #[cfg(feature = "t4d-bridge")]
            head_flip_after_read,
            #[cfg(feature = "t4d-bridge")]
            stale_code_index,
        }
    }

    fn assembly_fixture(
        pending_nonce: Option<PendingAccountNonce>,
        stale_on_read: Option<u64>,
    ) -> AssemblyFixture {
        assembly_fixture_with_read_delay(pending_nonce, stale_on_read, None)
    }

    fn production_source() -> &'static str {
        include_str!("tx_authority.rs").split("#[cfg(test)]").next().expect("production source")
    }

    fn probe() -> CancellationProbe {
        CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(1))),
            Arc::new(GlobalLifecycle::default()),
        )
    }

    fn encoded_victim(chain_id: u64, max_fee: u128, priority: u128) -> Bytes {
        let victim = TxEip1559 {
            chain_id,
            nonce: 1,
            gas_limit: 100_000,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority,
            to: TxKind::Call(Address::with_last_byte(28)),
            value: U256::ZERO,
            access_list: AccessList::default(),
            input: Bytes::new(),
        }
        .into_signed(Signature::new(U256::from(1), U256::from(2), false));
        Bytes::from(victim.encoded_2718())
    }

    fn captured_snapshot(
        parent_hash: B256,
        block_number: u64,
        base_fee: Option<u64>,
        pending_nonce: Option<PendingAccountNonce>,
        current: Arc<AtomicBool>,
    ) -> SnapshotHandle {
        let view: Arc<dyn PendingSnapshotView + Send + Sync> =
            Arc::new(TestSnapshotView { parent_hash, block_number, base_fee, pending_nonce });
        let port = TestSnapshotPort { view, received_at: Instant::now(), current };
        SnapshotCaptureCoordinator
            .capture(&port)
            .expect("snapshot capture")
            .expect("current snapshot")
    }

    fn derive_fixture_fees(
        fixture: &AssemblyFixture,
        snapshot: &SnapshotHandle,
        victim_raw: &Bytes,
        frame: MeasurementContext,
    ) -> Result<(u128, u128, u128), TxAuthorityError> {
        TxAuthorityAssembler::derive_fees(
            &AuthorityAssemblyView {
                snapshot,
                frame,
                prepared: &fixture.prepared,
                plan: &fixture.plan,
                victim_raw,
                probe: &fixture.probe,
            },
            frame,
        )
    }

    fn production_executor() -> DeployedContractIdentity {
        DeployedContractIdentity { address: EXECUTOR_ADDRESS, runtime_hash: EXECUTOR_RUNTIME_HASH }
    }

    fn prepared_pool() -> PreparedPoolState {
        PreparedPoolState {
            pool: Address::with_last_byte(1),
            protocol: ExactProtocol::UniswapV2,
            token0: Address::with_last_byte(2),
            token1: Address::with_last_byte(3),
            decimals0: 18,
            decimals1: 18,
            fee_pips: 3_000,
            quote: PreparedPoolQuote::constant_product(
                U256::from(1_000_000u64),
                U256::from(2_000_000u64),
            ),
        }
    }

    fn prepared_protocol_pool(protocol: ExactProtocol, identity: u8) -> PreparedPoolState {
        let quote = match protocol {
            ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile => {
                PreparedPoolQuote::constant_product(
                    U256::from(2_000_000u64),
                    U256::from(3_000_000u64),
                )
            }
            ExactProtocol::AerodromeStable => {
                PreparedPoolQuote::stable(U256::from(2_000_000u64), U256::from(3_000_000u64))
            }
            ExactProtocol::UniswapV3 => PreparedPoolQuote::v3(
                U256::from(1) << 96,
                U256::from(1_000_000_000u64),
                0,
                60,
                Vec::new(),
            ),
        };
        PreparedPoolState {
            pool: Address::with_last_byte(identity),
            protocol,
            token0: Address::with_last_byte(2),
            token1: Address::with_last_byte(3),
            decimals0: 18,
            decimals1: 18,
            fee_pips: 3_000,
            quote,
        }
    }

    #[test]
    fn t4b_mapping_is_total_and_aero_variants_share_exact_deployed_identity() {
        let mapping = ProtocolAdapterMapping::base_mainnet_pins();
        assert_eq!(
            mapping.resolve(ExactProtocol::UniswapV2).address(),
            address!("17314D6F1B7A7A67b91A6131E3AE635AF90bAe1b")
        );
        assert_eq!(
            mapping.resolve(ExactProtocol::UniswapV3).runtime_hash(),
            b256!("dd184f8bc9680971b835c0f05c4f5986845ba888db0857f04eb3c8cd4e44d93c")
        );
        assert_eq!(
            mapping.resolve(ExactProtocol::AerodromeVolatile),
            mapping.resolve(ExactProtocol::AerodromeStable)
        );
        assert!(!mapping.resolve(ExactProtocol::AerodromeStable).address().is_zero());
        assert_eq!(EXECUTOR_ADDRESS, address!("1810cbFA042e8199121021F056Afe8B31028CF55"));
        assert_eq!(
            EXECUTOR_RUNTIME_HASH,
            b256!("cc7e119d458f147a9baab30f51d4ef7fbfe6eef48377988d480f6e7b693e671d")
        );
        assert_eq!(EXECUTOR_SENDER, address!("98e1e2A84557D49496D1BFE31EA7b5a6C59FD0f9"));
        assert_eq!(
            mapping.resolve(ExactProtocol::UniswapV2).runtime_hash(),
            b256!("75f81d350eea433778932277055e2fb493f9954c307df0c0a7613418aa0ca2ae")
        );
        assert_eq!(
            mapping.resolve(ExactProtocol::UniswapV3).address(),
            address!("D73a2ACbd855AdA5bdF950E3D3796E0557504DfF")
        );
        assert_eq!(
            mapping.resolve(ExactProtocol::AerodromeVolatile).address(),
            address!("6a2242f52Db5aC8d6631AC7244Bb7fa370454F24")
        );
        assert_eq!(
            mapping.resolve(ExactProtocol::AerodromeVolatile).runtime_hash(),
            b256!("426950cd6948988dc7de442de3a3efcbc588670e29856b4bbd3cac70c374b8f5")
        );
    }

    #[test]
    fn t4b_identity_install_requires_executor_and_all_adapters_at_one_parent() {
        let installed = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        assert_eq!(installed.assembler.sender, Address::with_last_byte(23));
        assert_eq!(installed.assembler.executor.address(), Address::with_last_byte(29));
        let install_parent = installed.frame.parent_hash;
        let install_sender = installed.assembler.sender;
        let install_executor = installed.assembler.executor.clone();
        let install_adapters = installed.assembler.adapters;
        let install_contracts =
            TxAuthorityAssembler::contract_addresses(&install_executor, &install_adapters);
        let install_state = TxAuthorityStateRead::new(
            install_parent,
            Some(4),
            [
                Some(Bytes::from_static(b"executor")),
                Some(Bytes::from_static(b"uniswap-v2")),
                Some(Bytes::from_static(b"uniswap-v3")),
                Some(Bytes::from_static(b"aerodrome")),
            ],
        );
        for (state_error, head_flip_after_read) in [(true, false), (false, true)] {
            let node = Arc::new(TestNode {
                parent_hash: install_parent,
                sender: install_sender,
                contracts: install_contracts,
                state: install_state.clone(),
                current: Arc::new(AtomicBool::new(true)),
                reads: AtomicU64::new(0),
                stale_on_read: None,
                delay_on_read: None,
                state_error: Arc::new(AtomicBool::new(state_error)),
                head_flip_after_read: Arc::new(AtomicBool::new(head_flip_after_read)),
                stale_code_index: Arc::new(AtomicU64::new(u64::MAX)),
            });
            assert!(matches!(
                TxAuthorityAssembler::install(
                    node,
                    install_executor.clone(),
                    install_sender,
                    install_adapters.clone(),
                ),
                Err(TxAuthorityError::DeploymentIdentityRejected)
            ));
        }
        let mapping = ProtocolAdapterMapping::base_mainnet_pins();
        let parent = B256::with_last_byte(1);
        let missing = TxAuthorityStateRead::new(parent, Some(0), std::array::from_fn(|_| None));
        assert_eq!(
            TxAuthorityAssembler::validate_state_codes(
                &missing,
                parent,
                &production_executor(),
                &mapping,
            ),
            Err(TxAuthorityError::DeploymentIdentityRejected)
        );
        let empty =
            TxAuthorityStateRead::new(parent, Some(0), std::array::from_fn(|_| Some(Bytes::new())));
        assert_eq!(
            TxAuthorityAssembler::validate_state_codes(
                &empty,
                parent,
                &production_executor(),
                &mapping,
            ),
            Err(TxAuthorityError::DeploymentIdentityRejected)
        );
        let wrong_hash = TxAuthorityStateRead::new(
            parent,
            Some(0),
            std::array::from_fn(|_| Some(Bytes::from_static(b"wrong-runtime"))),
        );
        assert_eq!(
            TxAuthorityAssembler::validate_state_codes(
                &wrong_hash,
                parent,
                &production_executor(),
                &mapping,
            ),
            Err(TxAuthorityError::DeploymentIdentityRejected)
        );
        let partial = TxAuthorityStateRead::new(
            parent,
            Some(0),
            [
                Some(Bytes::from_static(b"executor")),
                Some(Bytes::from_static(b"uniswap-v2")),
                None,
                Some(Bytes::from_static(b"aerodrome")),
            ],
        );
        assert_eq!(
            TxAuthorityAssembler::validate_state_codes(
                &partial,
                parent,
                &production_executor(),
                &mapping,
            ),
            Err(TxAuthorityError::DeploymentIdentityRejected)
        );
        let wrong_parent = TxAuthorityStateRead::new(
            B256::with_last_byte(2),
            Some(0),
            std::array::from_fn(|_| Some(Bytes::from_static(b"code"))),
        );
        assert_eq!(
            TxAuthorityAssembler::validate_state_codes(
                &wrong_parent,
                parent,
                &production_executor(),
                &mapping,
            ),
            Err(TxAuthorityError::DeploymentIdentityRejected)
        );
    }

    #[test]
    fn t4b_hop_params_have_one_mapping_derived_constructor_and_no_adapter_injection() {
        let source = production_source();
        assert_eq!(source.matches("ProtocolAdapterMapping::base_mainnet_pins()").count(), 1);
        assert!(source.contains("struct ValidatedAbiHop {\n    adapter: Address,"));
        assert_eq!(source.matches("execution.adapters.resolve(hop.protocol)").count(), 1);
        assert!(!source.contains("pub adapter:"));
        assert!(!source.contains("pub(crate) adapter:"));
        assert!(!source.contains("set_adapter"));
    }

    #[test]
    fn t4b_requote_binds_both_hop_floors_to_same_prepared_frame() {
        let pool = prepared_pool();
        let hop = BackrunHop {
            pool: pool.pool,
            protocol: pool.protocol,
            token_in: pool.token0,
            token_out: pool.token1,
            fee_pips: pool.fee_pips,
        };
        let pools = vec![pool.clone()];
        let selected = TxAuthorityAssembler::unique_pool(&pools, &hop).expect("one exact pool");
        let output = selected
            .quote_exact_in(hop.token_in, U256::from(10_000u64), &probe())
            .expect("same-frame quote");
        assert!(!output.is_zero());
        for (identity, protocol) in [
            ExactProtocol::UniswapV2,
            ExactProtocol::UniswapV3,
            ExactProtocol::AerodromeVolatile,
            ExactProtocol::AerodromeStable,
        ]
        .into_iter()
        .enumerate()
        {
            let protocol_pool = prepared_protocol_pool(protocol, 40 + identity as u8);
            protocol_pool.validate().expect("protocol fixture validates");
            let protocol_hop = BackrunHop {
                pool: protocol_pool.pool,
                protocol,
                token_in: protocol_pool.token0,
                token_out: protocol_pool.token1,
                fee_pips: protocol_pool.fee_pips,
            };
            let exact = protocol_pool
                .quote_exact_in(protocol_hop.token_in, U256::from(1_000u64), &probe())
                .expect("exact protocol quote");
            assert!(!exact.is_zero(), "{protocol:?} quote");
            assert_eq!(
                TxAuthorityAssembler::unique_pool(
                    std::slice::from_ref(&protocol_pool),
                    &protocol_hop,
                ),
                Ok(&protocol_pool)
            );
        }
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let detail =
            fixture.assembler.assemble_view(fixture.view()).expect("two-hop same-frame assembly");
        assert_eq!(
            detail.observation().hop_protocols(),
            [ExactProtocol::UniswapV2, ExactProtocol::UniswapV2]
        );
        drop(detail);
        let duplicates = vec![pool.clone(), pool];
        assert_eq!(
            TxAuthorityAssembler::unique_pool(&duplicates, &hop),
            Err(TxAuthorityError::RequoteRejected)
        );
        let mut wrong_fee = hop;
        wrong_fee.fee_pips = wrong_fee.fee_pips.saturating_add(1);
        assert_eq!(
            TxAuthorityAssembler::unique_pool(&pools, &wrong_fee),
            Err(TxAuthorityError::RequoteRejected)
        );
        let mut wrong_token = hop;
        wrong_token.token_in = Address::with_last_byte(99);
        assert_eq!(
            TxAuthorityAssembler::unique_pool(&pools, &wrong_token),
            Err(TxAuthorityError::RequoteRejected)
        );
        let mut missing_pool = hop;
        missing_pool.pool = Address::with_last_byte(98);
        assert_eq!(
            TxAuthorityAssembler::unique_pool(&pools, &missing_pool),
            Err(TxAuthorityError::RequoteRejected)
        );
        let mut wrong_protocol = hop;
        wrong_protocol.protocol = ExactProtocol::AerodromeStable;
        assert_eq!(
            TxAuthorityAssembler::unique_pool(&pools, &wrong_protocol),
            Err(TxAuthorityError::RequoteRejected)
        );

        let mut stale = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        stale.prepared[0].quote =
            PreparedPoolQuote::constant_product(U256::from(1_000u64), U256::from(1_000u64));
        assert_eq!(
            TxAuthorityAssembler::requote(&stale.view()),
            Err(TxAuthorityError::RequoteRejected)
        );

        let expired_probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now())),
            Arc::new(GlobalLifecycle::default()),
        );
        let expired_view = AuthorityAssemblyView {
            snapshot: &fixture.snapshot,
            frame: fixture.frame,
            prepared: &fixture.prepared,
            plan: &fixture.plan,
            victim_raw: &fixture.victim_raw,
            probe: &expired_probe,
        };
        assert_eq!(
            TxAuthorityAssembler::requote(&expired_view),
            Err(TxAuthorityError::DeadlineNoShape)
        );
    }

    #[test]
    fn t4b_snapshot_nonce_uses_pending_overlay_or_committed_fallback_without_txpool() {
        let overlay = PendingAccountNonce::checked(4, 6).expect("coherent overlay");
        let overlay_fixture = assembly_fixture(Some(overlay), None);
        let overlay_detail = overlay_fixture
            .assembler
            .assemble_view(overlay_fixture.view())
            .expect("pending-overlay nonce");
        assert_eq!(overlay_detail.observation().nonce(), 6);
        drop(overlay_detail);

        let committed_fixture = assembly_fixture(None, None);
        let committed_detail = committed_fixture
            .assembler
            .assemble_view(committed_fixture.view())
            .expect("committed fallback nonce");
        assert_eq!(committed_detail.observation().nonce(), 4);
        drop(committed_detail);

        let mismatch_fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(3, 6).expect("locally coherent")),
            None,
        );
        assert!(matches!(
            mismatch_fixture.assembler.assemble_view(mismatch_fixture.view()),
            Err(TxAuthorityError::NonceWitnessUnavailable)
        ));
        assert!(PendingAccountNonce::checked(7, 6).is_err());
        assert!(!production_source().to_ascii_lowercase().contains("txpool"));
    }

    #[test]
    fn t4b_observation_guard_allows_exactly_one_live_unsigned_detail() {
        let held = Arc::new(AtomicBool::new(false));
        let assembler = TxAuthorityAssembler {
            node: Arc::new(NoopNode),
            executor: production_executor(),
            sender: EXECUTOR_SENDER,
            adapters: ProtocolAdapterMapping::base_mainnet_pins(),
            held: Arc::clone(&held),
        };
        let first = assembler.acquire_guard().expect("first guard");
        assert!(matches!(assembler.acquire_guard(), Err(TxAuthorityError::ObservationBusy)));
        drop(first);
        assert!(!held.load(Ordering::Acquire));
        assert!(assembler.acquire_guard().is_ok());
    }

    #[test]
    fn t4b_nonce_witness_revalidation_rejects_stale_snapshot_without_replacement_claims() {
        let stale_fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            Some(2),
        );
        assert!(matches!(
            stale_fixture.assembler.assemble_view(stale_fixture.view()),
            Err(TxAuthorityError::NonceWitnessStaleBeforePublish)
        ));

        let current_fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let output =
            current_fixture.assembler.assemble_view(current_fixture.view()).expect("fresh output");
        assert_eq!(output.validate_at_drain(), Ok(()));
        current_fixture.current.store(false, Ordering::Release);
        assert_eq!(output.validate_at_drain(), Err(TxAuthorityError::SnapshotStaleAtDrain));
        assert!(!production_source().to_ascii_lowercase().contains("replacement"));
    }

    #[test]
    fn t4b_fee_gas_and_deadline_are_node_local_and_checked() {
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let output = fixture.assembler.assemble_view(fixture.view()).expect("node-local fee shape");
        let observed = output.observation();
        assert_eq!(observed.chain_id(), CHAIN_ID_BASE);
        assert_eq!(observed.gas_limit(), T4B_EXECUTOR_GAS_LIMIT);
        assert_eq!(observed.base_fee(), 100);
        assert_eq!(observed.max_priority_fee_per_gas(), 10);
        assert_eq!(observed.max_fee_per_gas(), 110);
        assert_eq!(observed.valid_until_block(), fixture.frame.block_number);
        drop(output);

        let missing_base_fee = captured_snapshot(
            fixture.frame.parent_hash,
            fixture.frame.block_number,
            None,
            None,
            Arc::new(AtomicBool::new(true)),
        );
        assert_eq!(
            derive_fixture_fees(&fixture, &missing_base_fee, &fixture.victim_raw, fixture.frame,),
            Err(TxAuthorityError::FeeAuthorityRejected)
        );

        let wrong_header = captured_snapshot(
            fixture.frame.parent_hash,
            fixture.frame.block_number + 1,
            Some(100),
            None,
            Arc::new(AtomicBool::new(true)),
        );
        assert_eq!(
            derive_fixture_fees(&fixture, &wrong_header, &fixture.victim_raw, fixture.frame),
            Err(TxAuthorityError::FeeAuthorityRejected)
        );

        let cap_limited = encoded_victim(CHAIN_ID_BASE, 109, 10);
        let mut cap_frame = fixture.frame;
        cap_frame.victim = keccak256(&cap_limited);
        assert_eq!(
            derive_fixture_fees(&fixture, &fixture.snapshot, &cap_limited, cap_frame),
            Err(TxAuthorityError::FeeAuthorityRejected)
        );

        let wrong_chain = encoded_victim(1, 110, 10);
        let mut wrong_chain_frame = fixture.frame;
        wrong_chain_frame.victim = keccak256(&wrong_chain);
        assert_eq!(
            derive_fixture_fees(&fixture, &fixture.snapshot, &wrong_chain, wrong_chain_frame,),
            Err(TxAuthorityError::FeeAuthorityRejected)
        );

        let overflow_snapshot = captured_snapshot(
            fixture.frame.parent_hash,
            fixture.frame.block_number,
            Some(u64::MAX),
            None,
            Arc::new(AtomicBool::new(true)),
        );
        let overflow_victim = encoded_victim(CHAIN_ID_BASE, u128::MAX, u128::MAX);
        let mut overflow_frame = fixture.frame;
        overflow_frame.victim = keccak256(&overflow_victim);
        assert_eq!(
            derive_fixture_fees(&fixture, &overflow_snapshot, &overflow_victim, overflow_frame,),
            Err(TxAuthorityError::FeeAuthorityRejected)
        );

        let expired_probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now())),
            Arc::new(GlobalLifecycle::default()),
        );
        let expired_view = AuthorityAssemblyView {
            snapshot: &fixture.snapshot,
            frame: fixture.frame,
            prepared: &fixture.prepared,
            plan: &fixture.plan,
            victim_raw: &fixture.victim_raw,
            probe: &expired_probe,
        };
        assert!(matches!(
            fixture.assembler.assemble_view(expired_view),
            Err(TxAuthorityError::DeadlineNoShape)
        ));
        let delayed = assembly_fixture_with_read_delay(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
            Some((2, Duration::from_millis(20))),
        );
        let final_checkpoint_probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_millis(5))),
            Arc::new(GlobalLifecycle::default()),
        );
        let delayed_view = AuthorityAssemblyView {
            snapshot: &delayed.snapshot,
            frame: delayed.frame,
            prepared: &delayed.prepared,
            plan: &delayed.plan,
            victim_raw: &delayed.victim_raw,
            probe: &final_checkpoint_probe,
        };
        assert!(matches!(
            delayed.assembler.assemble_view(delayed_view),
            Err(TxAuthorityError::DeadlineNoShape)
        ));
        assert!(!delayed.assembler.held.load(Ordering::Acquire));
        assert!(!production_source().contains("std::env"));
    }

    #[test]
    fn t4b_assemble_validated_binds_every_unsigned_field_and_rejects_tamper() {
        let mut fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let expected_floors = TxAuthorityAssembler::requote(&fixture.view()).expect("exact floors");
        let output =
            fixture.assembler.assemble_view(fixture.view()).expect("validated unsigned shape");
        let observed = output.observation();
        assert_eq!(observed.frame(), fixture.frame);
        assert_eq!(observed.victim(), fixture.plan.victim);
        assert_eq!(observed.plan_digest(), fixture.plan.digest.0);
        assert_eq!(observed.nonce(), 5);
        assert_eq!(observed.chain_id(), CHAIN_ID_BASE);
        assert_eq!(observed.gas_limit(), T4B_EXECUTOR_GAS_LIMIT);
        assert_eq!(observed.max_fee_per_gas(), 110);
        assert_eq!(observed.max_priority_fee_per_gas(), 10);
        assert_eq!(observed.valid_until_block(), fixture.frame.block_number);
        assert_eq!(
            observed.hop_protocols(),
            [fixture.plan.route[0].protocol, fixture.plan.route[1].protocol]
        );
        assert_eq!(
            observed.hop_adapters(),
            [
                output.execution.adapters.resolve(fixture.plan.route[0].protocol).address,
                output.execution.adapters.resolve(fixture.plan.route[1].protocol).address,
            ]
        );
        assert_eq!(
            observed.hop_runtime_hashes(),
            [
                output.execution.adapters.resolve(fixture.plan.route[0].protocol).runtime_hash,
                output.execution.adapters.resolve(fixture.plan.route[1].protocol).runtime_hash,
            ]
        );
        assert_eq!(output.unsigned_tx.chain_id, CHAIN_ID_BASE);
        assert_eq!(output.unsigned_tx.nonce, 5);
        assert_eq!(output.unsigned_tx.to, TxKind::Call(output.execution.executor.address));
        assert_eq!(output.unsigned_tx.gas_limit, T4B_EXECUTOR_GAS_LIMIT);
        assert_eq!(output.unsigned_tx.max_fee_per_gas, 110);
        assert_eq!(output.unsigned_tx.max_priority_fee_per_gas, 10);
        assert!(output.unsigned_tx.value.is_zero());
        assert!(output.unsigned_tx.access_list.is_empty());
        assert_eq!(output.unsigned_tx.signature_hash(), observed.unsigned_signing_hash());
        assert!(!output.unsigned_tx.input.is_empty());
        for floor in expected_floors {
            let encoded = floor.to_be_bytes::<32>();
            assert!(
                output
                    .unsigned_tx
                    .input
                    .as_ref()
                    .windows(encoded.len())
                    .any(|window| window == encoded)
            );
        }
        let redacted = format!("{output:?}");
        assert!(!redacted.contains("unsigned_tx"));
        assert!(!redacted.contains("input:"));
        drop(output);
        fixture.frame.predecessor_index += 1;
        assert!(matches!(
            fixture.assembler.assemble_view(fixture.view()),
            Err(TxAuthorityError::PlanOrFrameRejected)
        ));
        fixture.frame.predecessor_index -= 1;

        fixture.plan.amount_out += U256::from(1);
        assert!(matches!(
            fixture.assembler.assemble_view(fixture.view()),
            Err(TxAuthorityError::PlanOrFrameRejected)
        ));
    }

    #[test]
    fn t4b_selected_output_contains_no_signature_valid_envelope_or_raw_secret() {
        let source = production_source();
        assert!(!source.contains("Signature"));
        assert!(!source.contains(".into_signed"));
        assert!(!source.contains("private_key"));
        assert!(!source.contains("raw_signed"));
        assert!(!source.contains("serde"));
        assert!(source.contains("TxEip1559"));
        assert!(source.contains("MeasurementNonceGuard"));
        assert_ne!(keccak256(b"unsigned"), B256::ZERO);
    }
    #[cfg(feature = "t4d-bridge")]
    #[test]
    fn t4d_bridge_consumes_t4b_unsigned_detail_by_value_without_tx_extraction() {
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let bridge = fixture.bridge();
        let candidate =
            bridge.assemble_sealed_for_test(fixture.view()).expect("opaque sealed candidate");
        assert_eq!(candidate.bindings().nonce(), 5);
        let redacted = format!("{candidate:?}");
        assert!(!redacted.contains("unsigned_tx"));
        assert!(!redacted.contains("input:"));
        drop(candidate);
        assert!(
            bridge.assemble_sealed_for_test(fixture.view()).is_ok(),
            "dropping the linear candidate must release the capacity-one guard"
        );
    }

    #[cfg(feature = "t4d-bridge")]
    #[test]
    fn t4d_bindings_preserve_frame_executor_and_route_adapter_identities() {
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let bridge = fixture.bridge();
        let candidate =
            bridge.assemble_sealed_for_test(fixture.view()).expect("opaque sealed candidate");
        let bindings = candidate.bindings();

        assert_eq!(bindings.frame(), fixture.frame);
        assert_eq!(bindings.victim(), fixture.plan.victim);
        assert_eq!(bindings.plan_digest(), fixture.plan.digest.0);
        assert_eq!(bindings.sender(), fixture.assembler.sender);
        assert_eq!(bindings.nonce(), 5);
        assert_eq!(bindings.valid_until_block(), fixture.frame.block_number);
        assert_eq!(bindings.validated_parent(), fixture.frame.parent_hash);
        assert_eq!(bindings.executor(), &fixture.assembler.executor);
        assert_eq!(
            bindings.route_protocols(),
            [fixture.plan.route[0].protocol, fixture.plan.route[1].protocol]
        );
        let route_adapters = bindings.route_adapters();
        assert_eq!(
            route_adapters[0],
            fixture.assembler.adapters.resolve(fixture.plan.route[0].protocol)
        );
        assert_eq!(
            route_adapters[1],
            fixture.assembler.adapters.resolve(fixture.plan.route[1].protocol)
        );
        assert_ne!(bindings.unsigned_signing_hash(), B256::ZERO);
    }

    #[cfg(feature = "t4d-bridge")]
    #[test]
    fn t4d_aero_variants_preserve_route_order_while_sharing_deployed_identity() {
        let mut fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        fixture.prepared[0].protocol = ExactProtocol::AerodromeVolatile;
        fixture.prepared[0].quote =
            PreparedPoolQuote::constant_product(U256::from(1_000_000u64), U256::from(4_000_000u64));
        fixture.prepared[1].protocol = ExactProtocol::AerodromeStable;
        fixture.prepared[1].quote =
            PreparedPoolQuote::stable(U256::from(4_000_000u64), U256::from(1_000_000u64));
        fixture.plan.route[0].protocol = ExactProtocol::AerodromeVolatile;
        fixture.plan.route[1].protocol = ExactProtocol::AerodromeStable;
        let first_out = fixture.prepared[0]
            .quote_exact_in(fixture.plan.route[0].token_in, fixture.plan.amount_in, &fixture.probe)
            .expect("volatile route quote");
        fixture.plan.amount_out = fixture.prepared[1]
            .quote_exact_in(fixture.plan.route[1].token_in, first_out, &fixture.probe)
            .expect("stable route quote");
        assert!(fixture.plan.amount_out > fixture.plan.amount_in);
        fixture.plan.gross_profit = fixture.plan.amount_out - fixture.plan.amount_in;
        fixture.plan.digest = MeasurementEncoder::digest(&fixture.plan).expect("plan digest");

        let bridge = fixture.bridge();
        let candidate =
            bridge.assemble_sealed_for_test(fixture.view()).expect("Aero route candidate");
        let bindings = candidate.bindings();
        assert_eq!(
            bindings.route_protocols(),
            [ExactProtocol::AerodromeVolatile, ExactProtocol::AerodromeStable]
        );
        let route_adapters = bindings.route_adapters();
        assert_eq!(route_adapters[0], route_adapters[1]);
        assert_eq!(
            route_adapters[0],
            fixture.assembler.adapters.resolve(ExactProtocol::AerodromeVolatile)
        );
    }

    #[cfg(feature = "t4d-bridge")]
    #[test]
    fn t4d_freshness_revalidates_executor_and_all_adapters_with_owned_provider() {
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let bridge = fixture.bridge();
        let candidate =
            bridge.assemble_sealed_for_test(fixture.view()).expect("opaque sealed candidate");
        assert!(revalidate_for_test(&fixture, &bridge, &candidate).is_ok());
        assert!(
            fixture.probe.token().complete(Instant::now(), true, fixture.probe.global()),
            "runtime completion should win before the control-task drain"
        );
        assert!(
            revalidate_for_test(&fixture, &bridge, &candidate).is_ok(),
            "a successfully completed producer lifecycle remains valid for shadow drain"
        );

        for identity in 0..4 {
            fixture.stale_code_index.store(identity, Ordering::Release);
            assert_eq!(
                revalidate_for_test(&fixture, &bridge, &candidate),
                Err(bridge::BridgeError::ExecutionIdentityChanged)
            );
            fixture.stale_code_index.store(u64::MAX, Ordering::Release);
        }

        fixture.state_error.store(true, Ordering::Release);
        assert_eq!(
            revalidate_for_test(&fixture, &bridge, &candidate),
            Err(bridge::BridgeError::ExecutionFreshnessUnavailable)
        );
        fixture.state_error.store(false, Ordering::Release);
        fixture.head_flip_after_read.store(true, Ordering::Release);
        assert_eq!(
            revalidate_for_test(&fixture, &bridge, &candidate),
            Err(bridge::BridgeError::ExecutionIdentityChanged)
        );
    }

    #[cfg(feature = "t4e-handoff")]
    #[test]
    fn t4e_bridge_join_preserves_identity_and_enforces_block_deadline() {
        let fresh = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let bridge = fresh.bridge();
        let candidate =
            bridge.assemble_sealed_for_test(fresh.view()).expect("fresh sealed candidate");
        let expected = candidate.bindings();
        let victim = expected.victim();
        let plan_digest = expected.plan_digest();
        let executor = expected.executor().address();
        let deadline = expected.valid_until_block();
        let signing_hash = expected.unsigned_signing_hash();
        let campaign_id = base_mev_trader::CampaignId::new([7; 32]);
        fresh.current_block.store(deadline - 1, Ordering::Release);
        let provider = fresh.block_provider();
        let checked = bridge
            .into_checked_candidate(candidate, campaign_id, &provider)
            .expect("committed head immediately before deadline");
        let identity = checked.identity();
        assert_eq!(identity.campaign_id(), campaign_id);
        assert_eq!(identity.victim(), victim);
        assert_eq!(identity.plan_digest(), plan_digest);
        assert_eq!(identity.amount(), fresh.plan.amount_in);
        assert_eq!(identity.executor(), executor);
        assert_eq!(checked.valid_until_block(), deadline);
        assert_eq!(checked.unsigned_signing_hash(), signing_hash);

        for current_block in [deadline, deadline + 1] {
            let expired = assembly_fixture(
                Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
                None,
            );
            let expired_bridge = expired.bridge();
            let expired_candidate = expired_bridge
                .assemble_sealed_for_test(expired.view())
                .expect("deadline candidate");
            expired.current_block.store(current_block, Ordering::Release);
            assert!(matches!(
                expired_bridge.into_checked_candidate(
                    expired_candidate,
                    campaign_id,
                    &expired.block_provider(),
                ),
                Err(bridge::BridgeError::DeadlineNoHandoff)
            ));
        }
    }

    #[cfg(feature = "t4e-handoff")]
    #[test]
    fn t4e_revalidation_fails_closed_when_committed_head_is_unavailable() {
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let bridge = fixture.bridge();
        let candidate = bridge.assemble_sealed_for_test(fixture.view()).expect("sealed candidate");
        fixture.block_unavailable.store(true, Ordering::Release);

        assert_eq!(
            bridge.revalidate_for_handoff(&candidate, &fixture.block_provider()),
            Err(bridge::BridgeError::ExecutionFreshnessUnavailable)
        );
    }

    #[cfg(feature = "t4d-bridge")]
    #[test]
    fn t4d_facade_rejects_cross_installation_candidate_and_provider_reinjection() {
        let fixture = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let issuing_bridge = fixture.bridge();
        let other_bridge = fixture.bridge();
        let candidate = issuing_bridge
            .assemble_sealed_for_test(fixture.view())
            .expect("opaque sealed candidate");
        assert_eq!(
            revalidate_for_test(&fixture, &other_bridge, &candidate),
            Err(bridge::BridgeError::CrossInstallation)
        );
        drop(candidate);

        #[cfg(feature = "t4e-handoff")]
        {
            let candidate = issuing_bridge
                .assemble_sealed_for_test(fixture.view())
                .expect("second opaque sealed candidate");
            assert!(matches!(
                other_bridge.into_checked_candidate(
                    candidate,
                    base_mev_trader::CampaignId::new([9; 32]),
                    &fixture.block_provider(),
                ),
                Err(bridge::BridgeError::CrossInstallation)
            ));
        }

        let bridge_ast =
            syn::parse_file(include_str!("tx_authority/bridge.rs")).expect("bridge source parses");
        let public_methods = |type_name: &str| {
            bridge_ast
                .items
                .iter()
                .filter_map(|item| match item {
                    syn::Item::Impl(item)
                        if item.trait_.is_none()
                            && matches!(
                                item.self_ty.as_ref(),
                                syn::Type::Path(path)
                                    if path.path.segments.last().is_some_and(
                                        |segment| segment.ident == type_name
                                    )
                            ) =>
                    {
                        Some(item)
                    }
                    _ => None,
                })
                .flat_map(|item| &item.items)
                .filter_map(|item| match item {
                    syn::ImplItem::Fn(method)
                        if matches!(method.vis, syn::Visibility::Public(_))
                            && !method.attrs.iter().any(|attr| attr.path().is_ident("cfg")) =>
                    {
                        Some(method.sig.ident.to_string())
                    }
                    _ => None,
                })
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            public_methods("InstalledSubmissionBridge"),
            BTreeSet::from([
                "assemble_sealed".to_owned(),
                "base_mainnet".to_owned(),
                "revalidate_for_handoff".to_owned(),
            ])
        );
        assert_eq!(
            public_methods("SealedUnsignedCandidate"),
            BTreeSet::from(["bindings".to_owned()])
        );
        assert_eq!(
            public_methods("AdapterAwareProofBindings"),
            BTreeSet::from([
                "executor".to_owned(),
                "frame".to_owned(),
                "nonce".to_owned(),
                "plan_digest".to_owned(),
                "route_adapters".to_owned(),
                "route_protocols".to_owned(),
                "sender".to_owned(),
                "unsigned_signing_hash".to_owned(),
                "valid_until_block".to_owned(),
                "validated_parent".to_owned(),
                "victim".to_owned(),
            ])
        );
    }

    #[cfg(feature = "t4d-bridge")]
    #[test]
    fn t4d_stale_cancelled_or_expired_candidate_emits_no_handoff() {
        let stale = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let stale_bridge = stale.bridge();
        let stale_candidate =
            stale_bridge.assemble_sealed_for_test(stale.view()).expect("stale candidate setup");
        stale.current.store(false, Ordering::Release);
        assert_eq!(
            revalidate_for_test(&stale, &stale_bridge, &stale_candidate),
            Err(bridge::BridgeError::SnapshotStale)
        );

        let cancelled = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        let cancelled_bridge = cancelled.bridge();
        let cancelled_candidate = cancelled_bridge
            .assemble_sealed_for_test(cancelled.view())
            .expect("cancelled candidate setup");
        cancelled.probe.token().request_cancel();
        assert_eq!(
            revalidate_for_test(&cancelled, &cancelled_bridge, &cancelled_candidate),
            Err(bridge::BridgeError::Cancelled)
        );

        let mut expired = assembly_fixture(
            Some(PendingAccountNonce::checked(4, 5).expect("pending nonce")),
            None,
        );
        expired.probe = CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_millis(50))),
            Arc::new(GlobalLifecycle::default()),
        );
        let expired_bridge = expired.bridge();
        let expired_candidate = expired_bridge
            .assemble_sealed_for_test(expired.view())
            .expect("expiring candidate setup");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            revalidate_for_test(&expired, &expired_bridge, &expired_candidate),
            Err(bridge::BridgeError::DeadlineNoHandoff)
        );
    }
}
