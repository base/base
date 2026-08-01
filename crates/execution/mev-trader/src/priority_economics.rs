//! Selected-route priority economics truth contract and raw-free wire ledger.
use std::{num::NonZeroUsize, sync::Mutex};

use alloy_primitives::{Address, B256, U256, U512, aliases::I512};
use serde::Serialize;
use thiserror::Error;

use crate::EconomicDispositionV1;

/// Wire schema identifier for selected-route priority economics.
pub const PRIORITY_ECONOMICS_SCHEMA_V2: &str = "base-mev/priority-economics/v2";

/// Ordered progress of the selected-route economics pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdmissionStageV2 {
    /// No pipeline work was started.
    NotRun,
    /// The pipeline was entered.
    PipelineStarted,
    /// Candidate discovery was attempted.
    CandidateDiscoveryAttempted,
    /// Candidate discovery completed and the candidate set is known.
    CandidateSetBuilt,
    /// The gross-ranked route was bound.
    RouteBound,
    /// The canonical candidate shape was built.
    ShapeBuilt,
    /// At least one exact economics authority was attempted.
    AuthorityAttempted,
    /// The checked economics leaf was evaluated.
    EconomicsEvaluated,
}

/// Closed top-level terminal taxonomy for priority economics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdmissionTerminalV2 {
    /// The pipeline did not run.
    PipelineNotRun,
    /// Discovery completed without a route.
    NoRoute,
    /// The deterministic gross winner was nonpositive.
    GrossNonpositive,
    /// A required exact authority was unavailable.
    AuthorityUnavailable,
    /// The selected gross winner had complete, nonpositive exact EV.
    SelectedRouteNoEdge,
    /// The selected gross winner had complete, strictly positive exact EV.
    SelectedRouteEvPositive,
}

/// Candidate-discovery failure reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiscoveryUnavailableReasonV2 {
    /// Snapshot-local candidate discovery failed.
    CandidateDiscoveryFailed,
    /// Candidate discovery exceeded its immutable deadline.
    Deadline,
    /// Candidate discovery was cancelled by a newer frame.
    Cancelled,
}

/// Failure before a canonical candidate shape existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PreShapeUnavailableReasonV2 {
    /// Selected-route binding failed.
    RouteBindingFailed,
    /// Calldata construction failed.
    CalldataConstructionFailed,
    /// Canonical envelope construction failed.
    EnvelopeConstructionFailed,
}

/// Failure after an exact authority was attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttemptedAuthorityUnavailableReasonV2 {
    /// Exact full-route quote authority failed.
    FullRouteQuoteUnavailable,
    /// Same-shape execution-gas authority failed.
    ExecutionGasUnavailable,
    /// Canonical-shape L1 fee authority failed.
    L1FeeUnavailable,
    /// A required active-fork fee was unavailable.
    RequiredFeeUnavailable,
    /// Checked leaf preconditions failed.
    LeafPreconditionFailed,
}

/// Exact discovery failure, with no downstream evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryAuthorityUnavailableV2 {
    reason: DiscoveryUnavailableReasonV2,
    discovery_digest: B256,
}

impl DiscoveryAuthorityUnavailableV2 {
    /// Creates a discovery failure bound to the attempted discovery inputs.
    pub fn new(
        reason: DiscoveryUnavailableReasonV2,
        discovery_digest: B256,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        if discovery_digest.is_zero() {
            return Err(PriorityEconomicsValidationErrorV2::ZeroDigest);
        }
        Ok(Self { reason, discovery_digest })
    }

    /// Returns the typed failure reason.
    pub const fn reason(&self) -> DiscoveryUnavailableReasonV2 {
        self.reason
    }

    /// Returns the discovery-input digest.
    pub const fn discovery_digest(&self) -> B256 {
        self.discovery_digest
    }
}

/// Exact pre-shape failure bound to the selected route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreShapeAuthorityUnavailableV2 {
    stage: AdmissionStageV2,
    reason: PreShapeUnavailableReasonV2,
    binding_digest: B256,
}

impl PreShapeAuthorityUnavailableV2 {
    /// Creates a failure occurring after route binding and before exact authority.
    pub fn new(
        stage: AdmissionStageV2,
        reason: PreShapeUnavailableReasonV2,
        binding_digest: B256,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        if stage != AdmissionStageV2::RouteBound {
            return Err(PriorityEconomicsValidationErrorV2::StageMismatch);
        }
        if binding_digest.is_zero() {
            return Err(PriorityEconomicsValidationErrorV2::ZeroDigest);
        }
        Ok(Self { stage, reason, binding_digest })
    }

    /// Returns the exact last stage.
    pub const fn stage(&self) -> AdmissionStageV2 {
        self.stage
    }
}

/// Exact post-attempt failure and the successfully observed values preceding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptedAuthorityUnavailableV2 {
    reason: AttemptedAuthorityUnavailableReasonV2,
    binding_digest: B256,
    amount_out_wei: Option<U256>,
    actual_gas_used: Option<u64>,
    l2_fee_wei: Option<U256>,
    l1_fee: Option<CanonicalL1FeeEvidenceV2>,
}

impl AttemptedAuthorityUnavailableV2 {
    /// Creates an attempted-authority failure without inventing values for the failed component.
    pub fn new(
        reason: AttemptedAuthorityUnavailableReasonV2,
        binding_digest: B256,
        amount_out_wei: Option<U256>,
        actual_gas_used: Option<u64>,
        l2_fee_wei: Option<U256>,
        l1_fee: Option<CanonicalL1FeeEvidenceV2>,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        if binding_digest.is_zero() {
            return Err(PriorityEconomicsValidationErrorV2::ZeroDigest);
        }
        let evidence =
            Self { reason, binding_digest, amount_out_wei, actual_gas_used, l2_fee_wei, l1_fee };
        evidence.validate()?;
        Ok(evidence)
    }

    /// Validates that the failed component has no fabricated success value.
    pub fn validate(&self) -> Result<(), PriorityEconomicsValidationErrorV2> {
        let failed_value_present = match self.reason {
            AttemptedAuthorityUnavailableReasonV2::FullRouteQuoteUnavailable => {
                self.amount_out_wei.is_some()
            }
            AttemptedAuthorityUnavailableReasonV2::ExecutionGasUnavailable => {
                self.actual_gas_used.is_some() || self.l2_fee_wei.is_some()
            }
            AttemptedAuthorityUnavailableReasonV2::L1FeeUnavailable => self.l1_fee.is_some(),
            AttemptedAuthorityUnavailableReasonV2::RequiredFeeUnavailable
            | AttemptedAuthorityUnavailableReasonV2::LeafPreconditionFailed => false,
        };
        if failed_value_present {
            return Err(PriorityEconomicsValidationErrorV2::FabricatedComponent);
        }
        Ok(())
    }
}

/// Nested authority-unavailable evidence; the shape identifies how far execution progressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "evidence")]
pub enum AuthorityUnavailableV2 {
    /// Candidate discovery itself failed.
    Discovery(DiscoveryAuthorityUnavailableV2),
    /// A selected route existed but no exact authority was attempted.
    PreShape(PreShapeAuthorityUnavailableV2),
    /// An exact authority was attempted and failed.
    Attempted(AttemptedAuthorityUnavailableV2),
}

/// Exact route identity for one gross-ranked winner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedRouteEvidenceV2 {
    victim_tx_hash: B256,
    directed_key: B256,
    pools: [Address; 2],
    tokens: [Address; 3],
    adapter_code_hashes: [B256; 2],
    hop_fees: [u32; 2],
    hop_zero_for_one: [bool; 2],
    amount_in_wei: U256,
    frame_digest: B256,
    parent_digest: B256,
    post_victim_state_digest: B256,
    request_digest: B256,
    calldata_digest: B256,
    envelope_digest: B256,
}

impl SelectedRouteEvidenceV2 {
    /// Creates a complete, non-sentinel route identity.
    pub fn new(
        victim_tx_hash: B256,
        directed_key: B256,
        pools: [Address; 2],
        tokens: [Address; 3],
        adapter_code_hashes: [B256; 2],
        hop_fees: [u32; 2],
        hop_zero_for_one: [bool; 2],
        amount_in_wei: U256,
        frame_digest: B256,
        parent_digest: B256,
        post_victim_state_digest: B256,
        request_digest: B256,
        calldata_digest: B256,
        envelope_digest: B256,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let route = Self {
            victim_tx_hash,
            directed_key,
            pools,
            tokens,
            adapter_code_hashes,
            hop_fees,
            hop_zero_for_one,
            amount_in_wei,
            frame_digest,
            parent_digest,
            post_victim_state_digest,
            request_digest,
            calldata_digest,
            envelope_digest,
        };
        route.validate()?;
        Ok(route)
    }

    /// Validates that no absence sentinel was substituted for route evidence.
    pub fn validate(&self) -> Result<(), PriorityEconomicsValidationErrorV2> {
        if self.victim_tx_hash.is_zero()
            || self.directed_key.is_zero()
            || self.adapter_code_hashes.iter().any(B256::is_zero)
            || [
                self.frame_digest,
                self.parent_digest,
                self.post_victim_state_digest,
                self.request_digest,
                self.calldata_digest,
                self.envelope_digest,
            ]
            .iter()
            .any(B256::is_zero)
        {
            return Err(PriorityEconomicsValidationErrorV2::ZeroDigest);
        }
        if self.pools.iter().any(|address| address.is_zero())
            || self.tokens.iter().any(|address| address.is_zero())
            || self.hop_fees.contains(&0)
            || self.amount_in_wei.is_zero()
        {
            return Err(PriorityEconomicsValidationErrorV2::IncompleteRoute);
        }
        Ok(())
    }

    /// Returns the victim transaction hash.
    pub const fn victim_tx_hash(&self) -> B256 {
        self.victim_tx_hash
    }

    /// Returns the selected input amount.
    pub const fn amount_in_wei(&self) -> U256 {
        self.amount_in_wei
    }

    /// Returns the shape binding digest used by authority evidence.
    pub const fn envelope_digest(&self) -> B256 {
        self.envelope_digest
    }
}

/// Raw-free canonical-shape OP L1 fee evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalL1FeeEvidenceV2 {
    encoded_length: u64,
    zero_bytes: u64,
    non_zero_bytes: u64,
    fast_lz_size: u64,
    digest: B256,
    fee: U256,
}

impl CanonicalL1FeeEvidenceV2 {
    /// Creates a checked canonical-shape fee tuple.
    pub fn new(
        encoded_length: u64,
        zero_bytes: u64,
        non_zero_bytes: u64,
        fast_lz_size: u64,
        digest: B256,
        fee: U256,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        if encoded_length == 0
            || zero_bytes.checked_add(non_zero_bytes) != Some(encoded_length)
            || fast_lz_size == 0
        {
            return Err(PriorityEconomicsValidationErrorV2::InvalidL1Tuple);
        }
        if digest.is_zero() {
            return Err(PriorityEconomicsValidationErrorV2::ZeroDigest);
        }
        Ok(Self { encoded_length, zero_bytes, non_zero_bytes, fast_lz_size, digest, fee })
    }

    /// Returns the exact canonical-shape L1 fee.
    pub const fn fee(&self) -> U256 {
        self.fee
    }
}

/// Exact accounting counts accompanying one immutable V2 terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityEconomicsCountersV2 {
    candidate_discovery_attempted: u64,
    candidate_discovery_succeeded: u64,
    candidates_considered: u64,
    routes_selected: u64,
    shape_attempted: u64,
    authority_attempted: u64,
    authority_succeeded: u64,
    authority_failed: u64,
}

impl PriorityEconomicsCountersV2 {
    /// Creates counters and enforces attempted = succeeded + failed.
    pub fn new(
        candidate_discovery_attempted: u64,
        candidate_discovery_succeeded: u64,
        candidates_considered: u64,
        routes_selected: u64,
        shape_attempted: u64,
        authority_attempted: u64,
        authority_succeeded: u64,
        authority_failed: u64,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let counters = Self {
            candidate_discovery_attempted,
            candidate_discovery_succeeded,
            candidates_considered,
            routes_selected,
            shape_attempted,
            authority_attempted,
            authority_succeeded,
            authority_failed,
        };
        counters.validate()?;
        Ok(counters)
    }

    /// Returns all-zero counters for a pipeline that did not run.
    pub const fn pipeline_not_run() -> Self {
        Self {
            candidate_discovery_attempted: 0,
            candidate_discovery_succeeded: 0,
            candidates_considered: 0,
            routes_selected: 0,
            shape_attempted: 0,
            authority_attempted: 0,
            authority_succeeded: 0,
            authority_failed: 0,
        }
    }
    /// Returns the eight counters in their canonical wire order.
    pub const fn values(&self) -> [u64; 8] {
        [
            self.candidate_discovery_attempted,
            self.candidate_discovery_succeeded,
            self.candidates_considered,
            self.routes_selected,
            self.shape_attempted,
            self.authority_attempted,
            self.authority_succeeded,
            self.authority_failed,
        ]
    }

    fn validate_exact(&self, expected: [u64; 8]) -> Result<(), PriorityEconomicsValidationErrorV2> {
        if self.values() != expected {
            return Err(PriorityEconomicsValidationErrorV2::CounterMismatch);
        }
        Ok(())
    }

    /// Validates conservation and route/candidate cardinality.
    pub fn validate(&self) -> Result<(), PriorityEconomicsValidationErrorV2> {
        if self.authority_attempted
            != self
                .authority_succeeded
                .checked_add(self.authority_failed)
                .ok_or(PriorityEconomicsValidationErrorV2::CounterMismatch)?
            || self.candidate_discovery_succeeded > self.candidate_discovery_attempted
            || self.routes_selected > self.candidates_considered
        {
            return Err(PriorityEconomicsValidationErrorV2::CounterMismatch);
        }
        Ok(())
    }

    /// Returns the number of selected routes.
    pub const fn routes_selected(&self) -> u64 {
        self.routes_selected
    }
}

/// Validation failure for the V2 priority-economics truth contract.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PriorityEconomicsValidationErrorV2 {
    /// A mandatory binding digest used the forbidden zero sentinel.
    #[error("priority economics contains a zero digest")]
    ZeroDigest,
    /// Route evidence was incomplete.
    #[error("priority economics route evidence is incomplete")]
    IncompleteRoute,
    /// Stage and terminal shape disagree.
    #[error("priority economics stage does not match its terminal")]
    StageMismatch,
    /// Counters do not conserve attempts or cardinality.
    #[error("priority economics counters do not conserve")]
    CounterMismatch,
    /// A failed component was assigned a fabricated success value.
    #[error("priority economics failed component has a value")]
    FabricatedComponent,
    /// The raw-free L1 tuple is internally inconsistent.
    #[error("priority economics L1 tuple is invalid")]
    InvalidL1Tuple,
    /// Signed economics or exact cost arithmetic disagrees.
    #[error("priority economics arithmetic is inconsistent")]
    ArithmeticMismatch,
    /// A terminal contains fields that must be null.
    #[error("priority economics null contract is invalid")]
    NullContract,
    /// Positive economics did not retain the mandatory optimism flag.
    #[error("positive priority economics must be gross-optimism-unverified")]
    OptimismFlagRequired,
}

/// Failure to retain or read the bounded production priority-economics ledger.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PriorityEconomicsLedgerErrorV2 {
    /// The fixed ledger capacity was reached; the record was not appended.
    #[error("priority economics ledger capacity {capacity} is exhausted")]
    CapacityExceeded {
        /// Configured maximum number of retained rows.
        capacity: usize,
    },
    /// A prior panic poisoned the ledger mutex.
    #[error("priority economics ledger lock is poisoned")]
    LockPoisoned,
    /// The proposed V2 row did not satisfy the truth contract.
    #[error("priority economics ledger rejected an invalid row: {0}")]
    Validation(#[from] PriorityEconomicsValidationErrorV2),
}

/// Bounded, append-only production owner for raw-free V2 priority-economics records.
///
/// Rows are retained independently of downstream admission. Capacity exhaustion and mutex
/// poisoning are fail-closed: the ledger never evicts, replaces, or silently drops a row.
#[derive(Debug)]
pub struct PriorityEconomicsLedgerV2 {
    capacity: NonZeroUsize,
    rows: Mutex<Vec<PriorityEconomicsV2>>,
}

impl PriorityEconomicsLedgerV2 {
    /// Creates an empty ledger with a fixed, type-checked nonzero capacity.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self { capacity, rows: Mutex::new(Vec::new()) }
    }

    /// Validates and appends one owned V2 row without eviction.
    pub fn append(
        &self,
        record: PriorityEconomicsV2,
    ) -> Result<(), PriorityEconomicsLedgerErrorV2> {
        record.validate()?;
        let mut rows =
            self.rows.lock().map_err(|_| PriorityEconomicsLedgerErrorV2::LockPoisoned)?;
        if rows.len() >= self.capacity.get() {
            return Err(PriorityEconomicsLedgerErrorV2::CapacityExceeded {
                capacity: self.capacity.get(),
            });
        }
        rows.push(record);
        Ok(())
    }

    /// Returns a cloned, point-in-time view of every retained row in append order.
    pub fn snapshot(&self) -> Result<Vec<PriorityEconomicsV2>, PriorityEconomicsLedgerErrorV2> {
        self.rows
            .lock()
            .map(|rows| rows.clone())
            .map_err(|_| PriorityEconomicsLedgerErrorV2::LockPoisoned)
    }

    /// Returns the immutable maximum number of retained rows.
    pub const fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }
}

/// Immutable V2 selected-route priority-economics record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityEconomicsV2 {
    schema_version: &'static str,
    stage: AdmissionStageV2,
    terminal: AdmissionTerminalV2,
    authority_unavailable: Option<AuthorityUnavailableV2>,
    route: Option<SelectedRouteEvidenceV2>,
    amount_out_wei: Option<U256>,
    gross_wei: Option<I512>,
    kickback_wei: Option<U256>,
    retained_wei: Option<U256>,
    actual_gas_used: Option<u64>,
    l2_fee_wei: Option<U256>,
    l1_fee: Option<CanonicalL1FeeEvidenceV2>,
    total_cost_wei: Option<U256>,
    ev_wei: Option<I512>,
    shortfall_wei: Option<U256>,
    net_ranked: bool,
    gross_optimism_unverified: Option<bool>,
    synthetic_reachability_only: Option<bool>,
    counters: PriorityEconomicsCountersV2,
}

impl PriorityEconomicsV2 {
    /// Constructs the unique pipeline-not-run record.
    pub const fn pipeline_not_run() -> Self {
        Self {
            schema_version: PRIORITY_ECONOMICS_SCHEMA_V2,
            stage: AdmissionStageV2::NotRun,
            terminal: AdmissionTerminalV2::PipelineNotRun,
            authority_unavailable: None,
            route: None,
            amount_out_wei: None,
            gross_wei: None,
            kickback_wei: None,
            retained_wei: None,
            actual_gas_used: None,
            l2_fee_wei: None,
            l1_fee: None,
            total_cost_wei: None,
            ev_wei: None,
            shortfall_wei: None,
            net_ranked: false,
            gross_optimism_unverified: None,
            synthetic_reachability_only: None,
            counters: PriorityEconomicsCountersV2::pipeline_not_run(),
        }
    }

    /// Constructs a completed discovery with no selected route.
    pub fn no_route(
        counters: PriorityEconomicsCountersV2,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let record = Self {
            stage: AdmissionStageV2::CandidateSetBuilt,
            terminal: AdmissionTerminalV2::NoRoute,
            counters,
            ..Self::pipeline_not_run()
        };
        record.validate()?;
        Ok(record)
    }

    /// Constructs a route whose signed gross is zero or negative.
    pub fn gross_nonpositive(
        route: SelectedRouteEvidenceV2,
        gross_wei: I512,
        counters: PriorityEconomicsCountersV2,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let record = Self {
            stage: AdmissionStageV2::RouteBound,
            terminal: AdmissionTerminalV2::GrossNonpositive,
            route: Some(route),
            gross_wei: Some(gross_wei),
            counters,
            ..Self::pipeline_not_run()
        };
        record.validate()?;
        Ok(record)
    }

    /// Constructs a typed exact-authority failure.
    pub fn authority_unavailable(
        stage: AdmissionStageV2,
        reason: AuthorityUnavailableV2,
        route: Option<SelectedRouteEvidenceV2>,
        gross_wei: Option<I512>,
        counters: PriorityEconomicsCountersV2,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let record = Self {
            stage,
            terminal: AdmissionTerminalV2::AuthorityUnavailable,
            authority_unavailable: Some(reason),
            route,
            gross_wei,
            counters,
            ..Self::pipeline_not_run()
        };
        record.validate()?;
        Ok(record)
    }

    /// Constructs an evaluated terminal from unsigned execution facts, deriving every signed
    /// value and conservation field with checked arithmetic.
    pub fn evaluated_from_execution(
        route: SelectedRouteEvidenceV2,
        amount_out_wei: U256,
        kickback_wei: U256,
        retained_wei: U256,
        actual_gas_used: u64,
        l2_fee_wei: U256,
        l1_fee: CanonicalL1FeeEvidenceV2,
        gross_optimism_unverified: Option<bool>,
        counters: PriorityEconomicsCountersV2,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let amount_out = I512::from_raw(U512::from(amount_out_wei));
        let amount_in = I512::from_raw(U512::from(route.amount_in_wei));
        let gross_wei = amount_out
            .checked_sub(amount_in)
            .ok_or(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)?;
        let total_cost_wei = l2_fee_wei
            .checked_add(l1_fee.fee())
            .ok_or(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)?;
        let ev_wei = I512::from_raw(U512::from(retained_wei))
            .checked_sub(I512::from_raw(U512::from(total_cost_wei)))
            .ok_or(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)?;
        let shortfall_wei = if ev_wei.is_positive() {
            U256::ZERO
        } else {
            I512::from_raw(U512::from(1_u64))
                .checked_sub(ev_wei)
                .and_then(|value| U256::checked_from_limbs_slice(value.into_raw().as_limbs()))
                .ok_or(PriorityEconomicsValidationErrorV2::ArithmeticMismatch)?
        };
        Self::evaluated(
            route,
            amount_out_wei,
            gross_wei,
            kickback_wei,
            retained_wei,
            actual_gas_used,
            l2_fee_wei,
            l1_fee,
            total_cost_wei,
            ev_wei,
            shortfall_wei,
            gross_optimism_unverified,
            counters,
        )
    }

    /// Constructs a fully evaluated selected-route terminal using checked signed arithmetic.
    pub fn evaluated(
        route: SelectedRouteEvidenceV2,
        amount_out_wei: U256,
        gross_wei: I512,
        kickback_wei: U256,
        retained_wei: U256,
        actual_gas_used: u64,
        l2_fee_wei: U256,
        l1_fee: CanonicalL1FeeEvidenceV2,
        total_cost_wei: U256,
        ev_wei: I512,
        shortfall_wei: U256,
        gross_optimism_unverified: Option<bool>,
        counters: PriorityEconomicsCountersV2,
    ) -> Result<Self, PriorityEconomicsValidationErrorV2> {
        let terminal = if ev_wei.is_positive() {
            AdmissionTerminalV2::SelectedRouteEvPositive
        } else {
            AdmissionTerminalV2::SelectedRouteNoEdge
        };
        let record = Self {
            stage: AdmissionStageV2::EconomicsEvaluated,
            terminal,
            authority_unavailable: None,
            route: Some(route),
            amount_out_wei: Some(amount_out_wei),
            gross_wei: Some(gross_wei),
            kickback_wei: Some(kickback_wei),
            retained_wei: Some(retained_wei),
            actual_gas_used: Some(actual_gas_used),
            l2_fee_wei: Some(l2_fee_wei),
            l1_fee: Some(l1_fee),
            total_cost_wei: Some(total_cost_wei),
            ev_wei: Some(ev_wei),
            shortfall_wei: Some(shortfall_wei),
            net_ranked: false,
            gross_optimism_unverified,
            synthetic_reachability_only: if terminal == AdmissionTerminalV2::SelectedRouteEvPositive
            {
                Some(true)
            } else {
                None
            },
            counters,
            schema_version: PRIORITY_ECONOMICS_SCHEMA_V2,
        };
        record.validate()?;
        Ok(record)
    }

    /// Validates the exhaustive stage, terminal, counter, null, and signed-arithmetic contract.
    pub fn validate(&self) -> Result<(), PriorityEconomicsValidationErrorV2> {
        self.counters.validate()?;
        if self.schema_version != PRIORITY_ECONOMICS_SCHEMA_V2 || self.net_ranked {
            return Err(PriorityEconomicsValidationErrorV2::NullContract);
        }
        match self.terminal {
            AdmissionTerminalV2::PipelineNotRun => {
                self.counters.validate_exact([0, 0, 0, 0, 0, 0, 0, 0])?;
                if self.stage != AdmissionStageV2::NotRun || !self.only_empty_economics() {
                    return Err(PriorityEconomicsValidationErrorV2::NullContract);
                }
            }
            AdmissionTerminalV2::NoRoute => {
                self.counters.validate_exact([1, 1, 0, 0, 0, 0, 0, 0])?;
                if self.stage != AdmissionStageV2::CandidateSetBuilt
                    || self.route.is_some()
                    || !self.only_empty_economics()
                {
                    return Err(PriorityEconomicsValidationErrorV2::NullContract);
                }
            }
            AdmissionTerminalV2::GrossNonpositive => {
                self.counters.validate_exact([1, 1, 1, 1, 0, 0, 0, 0])?;
                if self.stage != AdmissionStageV2::RouteBound
                    || self.route.is_none()
                    || self.gross_wei.is_none_or(|gross| gross.is_positive())
                    || self.has_downstream_economics()
                {
                    return Err(PriorityEconomicsValidationErrorV2::NullContract);
                }
            }
            AdmissionTerminalV2::AuthorityUnavailable => self.validate_unavailable()?,
            AdmissionTerminalV2::SelectedRouteNoEdge
            | AdmissionTerminalV2::SelectedRouteEvPositive => self.validate_evaluated()?,
        }
        Ok(())
    }

    fn only_empty_economics(&self) -> bool {
        self.authority_unavailable.is_none()
            && self.route.is_none()
            && self.gross_wei.is_none()
            && !self.has_downstream_economics()
    }

    fn has_downstream_economics(&self) -> bool {
        self.amount_out_wei.is_some()
            || self.kickback_wei.is_some()
            || self.retained_wei.is_some()
            || self.actual_gas_used.is_some()
            || self.l2_fee_wei.is_some()
            || self.l1_fee.is_some()
            || self.total_cost_wei.is_some()
            || self.ev_wei.is_some()
            || self.shortfall_wei.is_some()
            || self.gross_optimism_unverified.is_some()
            || self.synthetic_reachability_only.is_some()
    }

    fn validate_unavailable(&self) -> Result<(), PriorityEconomicsValidationErrorV2> {
        if self.has_downstream_economics() {
            return Err(PriorityEconomicsValidationErrorV2::NullContract);
        }
        match self.authority_unavailable.as_ref() {
            Some(AuthorityUnavailableV2::Discovery(_)) => {
                self.counters.validate_exact([1, 0, 0, 0, 0, 0, 0, 0])?;
                if self.stage != AdmissionStageV2::CandidateDiscoveryAttempted
                    || self.route.is_some()
                    || self.gross_wei.is_some()
                {
                    return Err(PriorityEconomicsValidationErrorV2::StageMismatch);
                }
            }
            Some(AuthorityUnavailableV2::PreShape(evidence)) => {
                self.counters.validate_exact([1, 1, 1, 1, 0, 0, 0, 0])?;
                if evidence.stage != self.stage
                    || self.stage != AdmissionStageV2::RouteBound
                    || self.route.is_none()
                    || self.gross_wei.is_none_or(|gross| !gross.is_positive())
                {
                    return Err(PriorityEconomicsValidationErrorV2::StageMismatch);
                }
            }
            Some(AuthorityUnavailableV2::Attempted(evidence)) => {
                evidence.validate()?;
                self.counters.validate_exact([1, 1, 1, 1, 1, 1, 0, 1])?;
                if self.stage != AdmissionStageV2::AuthorityAttempted
                    || self.route.is_none()
                    || self.gross_wei.is_none_or(|gross| !gross.is_positive())
                {
                    return Err(PriorityEconomicsValidationErrorV2::StageMismatch);
                }
            }
            None => return Err(PriorityEconomicsValidationErrorV2::NullContract),
        }
        Ok(())
    }

    fn validate_evaluated(&self) -> Result<(), PriorityEconomicsValidationErrorV2> {
        self.counters.validate_exact([1, 1, 1, 1, 1, 1, 1, 0])?;
        if self.stage != AdmissionStageV2::EconomicsEvaluated
            || self.authority_unavailable.is_some()
            || self.route.is_none()
        {
            return Err(PriorityEconomicsValidationErrorV2::StageMismatch);
        }
        let (
            Some(route),
            Some(amount_out),
            Some(gross),
            Some(kickback),
            Some(retained),
            Some(actual_gas_used),
            Some(l2),
            Some(l1),
            Some(total),
            Some(ev),
            Some(shortfall),
        ) = (
            self.route.as_ref(),
            self.amount_out_wei,
            self.gross_wei,
            self.kickback_wei,
            self.retained_wei,
            self.actual_gas_used,
            self.l2_fee_wei,
            self.l1_fee.as_ref(),
            self.total_cost_wei,
            self.ev_wei,
            self.shortfall_wei,
        )
        else {
            return Err(PriorityEconomicsValidationErrorV2::NullContract);
        };
        if actual_gas_used == 0
            || !gross.is_positive()
            || I512::from_raw(U512::from(amount_out))
                .checked_sub(I512::from_raw(U512::from(route.amount_in_wei)))
                != Some(gross)
            || kickback.checked_add(retained).map(|value| I512::from_raw(U512::from(value)))
                != Some(gross)
            || l2.checked_add(l1.fee()) != Some(total)
            || I512::from_raw(U512::from(retained)).checked_sub(I512::from_raw(U512::from(total)))
                != Some(ev)
        {
            return Err(PriorityEconomicsValidationErrorV2::ArithmeticMismatch);
        }
        match self.terminal {
            AdmissionTerminalV2::SelectedRouteEvPositive => {
                if self.synthetic_reachability_only != Some(true) {
                    return Err(PriorityEconomicsValidationErrorV2::OptimismFlagRequired);
                }
                if !ev.is_positive()
                    || shortfall != U256::ZERO
                    || self.gross_optimism_unverified != Some(true)
                {
                    return Err(PriorityEconomicsValidationErrorV2::OptimismFlagRequired);
                }
            }
            AdmissionTerminalV2::SelectedRouteNoEdge => {
                if self.synthetic_reachability_only.is_some()
                    || self.gross_optimism_unverified.is_some()
                {
                    return Err(PriorityEconomicsValidationErrorV2::ArithmeticMismatch);
                }
                let expected_shortfall = I512::from_raw(U512::from(1_u64))
                    .checked_sub(ev)
                    .and_then(|value| U256::checked_from_limbs_slice(value.into_raw().as_limbs()));
                if ev.is_positive() || expected_shortfall != Some(shortfall) {
                    return Err(PriorityEconomicsValidationErrorV2::ArithmeticMismatch);
                }
            }
            _ => return Err(PriorityEconomicsValidationErrorV2::StageMismatch),
        }
        Ok(())
    }

    /// Returns the exact terminal stage.
    pub const fn stage(&self) -> AdmissionStageV2 {
        self.stage
    }

    /// Returns typed authority-unavailable evidence, when present.
    pub const fn authority_unavailable_reason(&self) -> Option<&AuthorityUnavailableV2> {
        self.authority_unavailable.as_ref()
    }

    /// Returns selected-route evidence, when a route was bound.
    pub const fn route(&self) -> Option<&SelectedRouteEvidenceV2> {
        self.route.as_ref()
    }

    /// Returns the exact terminal counters.
    pub const fn counters(&self) -> PriorityEconomicsCountersV2 {
        self.counters
    }
    /// Returns the closed terminal.
    pub const fn terminal(&self) -> AdmissionTerminalV2 {
        self.terminal
    }
    /// Returns the selected input amount when route evidence exists.
    pub const fn amount_in_wei(&self) -> Option<U256> {
        match self.route.as_ref() {
            Some(route) => Some(route.amount_in_wei),
            None => None,
        }
    }

    /// Returns the exact signed EV when evaluation completed.
    pub const fn ev_wei(&self) -> Option<I512> {
        self.ev_wei
    }

    /// Returns the mandatory false net-ranking claim.
    pub const fn net_ranked(&self) -> bool {
        self.net_ranked
    }

    /// Returns whether the record is explicitly limited to synthetic reachability.
    pub const fn synthetic_reachability_only(&self) -> Option<bool> {
        self.synthetic_reachability_only
    }

    /// Returns a deliberately lossy V1-compatible economics projection.
    pub fn project_v1(&self) -> PriorityEconomicsV1 {
        PriorityEconomicsV1 {
            disposition: match self.terminal {
                AdmissionTerminalV2::PipelineNotRun => EconomicDispositionV1::NotReached,
                AdmissionTerminalV2::NoRoute => EconomicDispositionV1::NoRoute,
                AdmissionTerminalV2::GrossNonpositive => EconomicDispositionV1::GrossNonpositive,
                AdmissionTerminalV2::AuthorityUnavailable => {
                    EconomicDispositionV1::AuthorityUnavailable
                }
                AdmissionTerminalV2::SelectedRouteNoEdge => EconomicDispositionV1::EvNonpositive,
                AdmissionTerminalV2::SelectedRouteEvPositive => EconomicDispositionV1::EvPositive,
            },
            gross_wei_signed: self.gross_wei.map(|value| value.to_string()),
            retained_wei: self.retained_wei.map(|value| value.to_string()),
            total_cost_wei: self.total_cost_wei.map(|value| value.to_string()),
            ev_wei_signed: self.ev_wei.map(|value| value.to_string()),
            shortfall_wei: self.shortfall_wei.map(|value| value.to_string()),
            candidates_considered: self.counters.candidates_considered,
            routes_selected: self.counters.routes_selected,
            authority_attempted: self.counters.authority_attempted,
            authority_succeeded: self.counters.authority_succeeded,
            authority_failed: self.counters.authority_failed,
        }
    }
}

/// Lossy V1 economics projection preserving V2 null and counter semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityEconomicsV1 {
    disposition: EconomicDispositionV1,
    gross_wei_signed: Option<String>,
    retained_wei: Option<String>,
    total_cost_wei: Option<String>,
    ev_wei_signed: Option<String>,
    shortfall_wei: Option<String>,
    candidates_considered: u64,
    routes_selected: u64,
    authority_attempted: u64,
    authority_succeeded: u64,
    authority_failed: u64,
}

impl PriorityEconomicsV1 {
    /// Returns the legacy closed disposition.
    pub const fn disposition(&self) -> EconomicDispositionV1 {
        self.disposition
    }

    /// Returns the signed decimal EV without collapsing zero or negative values.
    pub fn ev_wei_signed(&self) -> Option<&str> {
        self.ev_wei_signed.as_deref()
    }

    /// Returns the number of exact authority attempts.
    pub const fn authority_attempted(&self) -> u64 {
        self.authority_attempted
    }
}
