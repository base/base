//! Runtime-selected two-channel transport. [`send_gated`] is the SINGLE entry
//! point: it consumes a [`SubmissionAttempt`], re-validates the complete freshness
//! conjunction, and defaults to [`SimBackend`]. The live branch additionally
//! evaluates all four locks before minting a linear [`LiveEgressPermit`].
//! [`ProdBackend::execute`] remains the sole real network call site and compiles
//! only under `arm-live-egress` + `not(test)`.

use alloy_primitives::{B256, U256, keccak256};

use super::proofs::ProviderError;
use super::request::RequestSpec;
#[cfg(all(feature = "arm-live-egress", not(test)))]
use super::witness::FreshnessProof;
use super::witness::{
    FreshnessSources, PairedSubmission, ProofBindings, ValidatedExecutionIdentity,
};

/// A single submission attempt (by value; there is no separate retry fn).
// The `Initial` variant is intentionally larger (it owns both channel specs): a
// submission is single-shot and consumed immediately, so boxing would add a heap
// allocation to the hot submit path for no benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SubmissionAttempt {
    /// A fresh two-channel submission.
    Initial(PairedSubmission),
    /// An attribution-only retry (inclusion already landed).
    AttributionRetry(AttributionRetryToken),
}

/// Whether a simulation record represents an initial send or attribution retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationAttempt {
    /// Initial inclusion plus attribution.
    Initial,
    /// Attribution-only retry.
    AttributionRetry,
}

/// Stable join key for one candidate and signed simulation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationCorrelationKey(B256);

impl SimulationCorrelationKey {
    /// Returns the domain-separated key bytes.
    pub const fn value(self) -> B256 {
        self.0
    }
}

/// Inert requests produced by the production simulation backend.
#[derive(Debug)]
pub struct SimulationRecord {
    attempt: SimulationAttempt,
    requests: Vec<RequestSpec>,
    inclusion_receipt_hash: B256,
    correlation_key: SimulationCorrelationKey,
    campaign_id: base_mev_trader::CampaignId,
    victim_tx_hash: B256,
    plan_digest: B256,
    executor: alloy_primitives::Address,
    economics: Option<crate::PriorityEconomicsReceipt>,
    simulation_evidence: Option<super::witness::SimulationIdentityEvidence>,
    deployment_code_hash: B256,
    deployment_digest: B256,
    binary_digest: B256,
    r9_store_identity: B256,
    proof_valid_until_block: u64,
}

impl SimulationRecord {
    /// The simulated attempt kind.
    pub const fn attempt(&self) -> SimulationAttempt {
        self.attempt
    }

    /// Exact inert request specifications. No request has been sent.
    pub fn requests(&self) -> &[RequestSpec] {
        &self.requests
    }

    /// Expected inclusion hash, or the receipt hash binding an attribution retry.
    pub const fn inclusion_receipt_hash(&self) -> B256 {
        self.inclusion_receipt_hash
    }

    /// Stable key joining this attempt to its candidate and signed transaction.
    pub const fn correlation_key(&self) -> SimulationCorrelationKey {
        self.correlation_key
    }

    /// Campaign that produced this attempt.
    pub const fn campaign_id(&self) -> base_mev_trader::CampaignId {
        self.campaign_id
    }

    /// Victim transaction hash bound by the candidate.
    pub const fn victim_tx_hash(&self) -> B256 {
        self.victim_tx_hash
    }

    pub(crate) const fn executor(&self) -> alloy_primitives::Address {
        self.executor
    }

    /// Plan digest bound by the candidate.
    pub const fn plan_digest(&self) -> B256 {
        self.plan_digest
    }

    /// Checked economics from the sole positive-EV evaluator.
    pub const fn economics(&self) -> Option<crate::PriorityEconomicsReceipt> {
        self.economics
    }

    pub(crate) const fn simulation_evidence(
        &self,
    ) -> Option<super::witness::SimulationIdentityEvidence> {
        self.simulation_evidence
    }

    pub(crate) const fn deployment_code_hash(&self) -> B256 {
        self.deployment_code_hash
    }

    pub(crate) const fn deployment_digest(&self) -> B256 {
        self.deployment_digest
    }

    pub(crate) const fn binary_digest(&self) -> B256 {
        self.binary_digest
    }

    pub(crate) const fn r9_store_identity(&self) -> B256 {
        self.r9_store_identity
    }

    pub(crate) const fn proof_valid_until_block(&self) -> u64 {
        self.proof_valid_until_block
    }

    #[cfg(test)]
    pub(crate) fn for_store_test(economics: crate::PriorityEconomicsReceipt) -> Self {
        let campaign_id = base_mev_trader::CampaignId::new([2_u8; 32]);
        let victim_tx_hash = B256::repeat_byte(3);
        let plan_digest = B256::repeat_byte(4);
        let inclusion_receipt_hash = B256::repeat_byte(5);
        let id = ValidatedExecutionIdentity::for_simulation_store_test(
            campaign_id,
            victim_tx_hash,
            plan_digest,
            economics,
        );
        Self {
            attempt: SimulationAttempt::Initial,
            requests: vec![
                RequestSpec::for_simulation_store_test(super::request::Channel::Inclusion),
                RequestSpec::for_simulation_store_test(super::request::Channel::Attribution),
            ],
            inclusion_receipt_hash,
            correlation_key: simulation_correlation(&id, inclusion_receipt_hash),
            campaign_id,
            victim_tx_hash,
            plan_digest,
            executor: alloy_primitives::Address::ZERO,
            economics: Some(economics),
            simulation_evidence: id.simulation_evidence(),
            deployment_code_hash: B256::repeat_byte(14),
            deployment_digest: B256::repeat_byte(15),
            binary_digest: B256::repeat_byte(16),
            r9_store_identity: B256::repeat_byte(17),
            proof_valid_until_block: economics.authority_block + 1,
        }
    }
}

fn simulation_correlation(
    id: &ValidatedExecutionIdentity,
    signed_tx_hash: B256,
) -> SimulationCorrelationKey {
    const DOMAIN: &[u8] = b"base-mev/simulation-correlation/v1";
    let mut bytes = Vec::with_capacity(DOMAIN.len() + 32 * 4);
    bytes.extend_from_slice(DOMAIN);
    bytes.extend_from_slice(id.campaign_id().as_bytes());
    bytes.extend_from_slice(id.victim().as_slice());
    bytes.extend_from_slice(id.plan_digest().as_slice());
    bytes.extend_from_slice(signed_tx_hash.as_slice());
    SimulationCorrelationKey(keccak256(bytes))
}

/// A closed live lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveLockClosed {
    /// No explicit live runtime selection was consumed.
    ExplicitLiveSelection,
    /// The signed live-run receipt was not fresh.
    SignedReceipt,
    /// The authoritative kill state was not clear.
    KillAnchor,
    /// The funded account was absent.
    FundedAccountAbsent,
    /// The canonical balance authority failed.
    FundsUnavailable,
    /// Present hot-wallet balance exceeded the signed cap.
    FundsCapExceeded,
}

/// The typed outcome of a gated submission.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SubmitOutcome {
    /// The egress-moment re-validation failed (or the process was poisoned).
    NoEgress,
    /// The four live locks did not all hold.
    LiveLocksClosed(LiveLockClosed),
    /// No transport was attempted; exact inert requests are returned by value.
    Simulated(SimulationRecord),
    /// The live inclusion channel failed.
    InclusionFailed,
    /// Live inclusion landed but attribution failed.
    InclusionSentAttributionFailed(AttributionRetryToken),
    /// Both live channels succeeded.
    LiveComplete,
}

/// An attribution-only retry permit. It preserves the FULL freshness bindings +
/// identity by ownership, so a retry still runs the entire `send_gated`
/// re-validation before re-sending attribution. Private fields; minted only by a
/// backend on a partial failure.
#[derive(Debug)]
pub struct AttributionRetryToken {
    attribution: RequestSpec,
    bindings: ProofBindings,
    id: ValidatedExecutionIdentity,
    inclusion_receipt_hash: B256,
}

impl AttributionRetryToken {
    /// Mint a retry permit (backend-only, on inclusion-sent + attribution-failed).
    pub const fn new(
        attribution: RequestSpec,
        bindings: ProofBindings,
        id: ValidatedExecutionIdentity,
        inclusion_receipt_hash: B256,
    ) -> Self {
        Self { attribution, bindings, id, inclusion_receipt_hash }
    }

    /// The inclusion receipt hash the retry is bound to.
    pub const fn inclusion_receipt_hash(&self) -> B256 {
        self.inclusion_receipt_hash
    }
}

/// A linear egress permit. Constructed ONLY by [`send_gated`] after a full fresh
/// re-validation, and consumed by a single [`RawBackend::execute`] call.
#[derive(Debug)]
pub struct RawEgress {
    plan: EgressPlan,
}

/// What a [`RawEgress`] permit authorizes.
#[derive(Debug)]
pub enum EgressPlan {
    /// Initial two-channel send.
    Initial {
        /// The inclusion-channel request (`eth_sendRawTransaction`).
        inclusion: RequestSpec,
        /// The attribution-channel request (`eth_sendBundle`).
        attribution: RequestSpec,
        /// The captured proof bindings (for the retry token on partial failure).
        bindings: ProofBindings,
        /// The execution identity (for the retry token on partial failure).
        id: ValidatedExecutionIdentity,
        /// The expected inclusion tx hash (compared to the node-returned hash).
        expected_inclusion_hash: B256,
    },
    /// Attribution-only re-send.
    AttributionOnly {
        /// The attribution-channel request to re-send.
        attribution: RequestSpec,
        /// The captured proof bindings.
        bindings: ProofBindings,
        /// The execution identity.
        id: ValidatedExecutionIdentity,
        /// The inclusion receipt hash the retry is bound to.
        inclusion_receipt_hash: B256,
    },
}

impl RawEgress {
    /// Consume the permit into its plan (backend-only).
    pub fn into_plan(self) -> EgressPlan {
        self.plan
    }
}

mod sealed {
    #[allow(unnameable_types)]
    /// Sealed supertrait: no crate outside this module can implement the backend.
    pub trait Sealed {}
}

/// A backend permit. Its payloads have private fields and cannot be forged.
#[derive(Debug)]
pub enum BackendPermit {
    /// Simulation-only permit.
    Simulated(SimEgressPermit),
    /// Live-only permit.
    #[cfg(all(feature = "arm-live-egress", not(test)))]
    Live(LiveEgressPermit),
}

/// Linear simulation permit, minted only by [`send_gated`].
#[derive(Debug)]
pub struct SimEgressPermit {
    private: (),
}

/// Linear live permit, minted only after all four locks hold.
#[cfg(all(feature = "arm-live-egress", not(test)))]
#[derive(Debug)]
pub struct LiveEgressPermit {
    private: (),
}

/// Sealed low-level backend.
pub trait RawBackend: sealed::Sealed {
    /// Consume the matching permit and egress plan.
    fn execute(&self, permit: BackendPermit, egress: RawEgress) -> SubmitOutcome;
}

/// Production simulation backend. It records inert requests and opens no socket.
#[derive(Debug, Default)]
pub struct SimBackend;

impl sealed::Sealed for SimBackend {}

impl RawBackend for SimBackend {
    fn execute(&self, permit: BackendPermit, egress: RawEgress) -> SubmitOutcome {
        if !matches!(permit, BackendPermit::Simulated(SimEgressPermit { private: () })) {
            return SubmitOutcome::NoEgress;
        }
        let record = match egress.into_plan() {
            EgressPlan::Initial {
                inclusion,
                attribution,
                bindings,
                id,
                expected_inclusion_hash,
            } => SimulationRecord {
                attempt: SimulationAttempt::Initial,
                requests: vec![inclusion, attribution],
                inclusion_receipt_hash: expected_inclusion_hash,
                correlation_key: simulation_correlation(&id, expected_inclusion_hash),
                campaign_id: id.campaign_id(),
                victim_tx_hash: id.victim(),
                plan_digest: id.plan_digest(),
                executor: id.executor(),
                economics: id.economics(),
                simulation_evidence: id.simulation_evidence(),
                deployment_code_hash: bindings.deployment_code_hash(),
                deployment_digest: bindings.deployment_digest(),
                binary_digest: bindings.binary_digest(),
                r9_store_identity: B256::from(*bindings.r9_store_identity().as_bytes()),
                proof_valid_until_block: bindings.valid_until_block(),
            },
            EgressPlan::AttributionOnly { attribution, bindings, id, inclusion_receipt_hash } => {
                SimulationRecord {
                    attempt: SimulationAttempt::AttributionRetry,
                    requests: vec![attribution],
                    inclusion_receipt_hash,
                    correlation_key: simulation_correlation(&id, inclusion_receipt_hash),
                    campaign_id: id.campaign_id(),
                    victim_tx_hash: id.victim(),
                    plan_digest: id.plan_digest(),
                    executor: id.executor(),
                    economics: id.economics(),
                    simulation_evidence: id.simulation_evidence(),
                    deployment_code_hash: bindings.deployment_code_hash(),
                    deployment_digest: bindings.deployment_digest(),
                    binary_digest: bindings.binary_digest(),
                    r9_store_identity: B256::from(*bindings.r9_store_identity().as_bytes()),
                    proof_valid_until_block: bindings.valid_until_block(),
                }
            }
        };
        SubmitOutcome::Simulated(record)
    }
}

/// Non-`Clone` runtime backend selection.
#[derive(Debug)]
pub struct RuntimeBackend<'a> {
    inner: RuntimeBackendKind<'a>,
}

#[derive(Debug)]
enum RuntimeBackendKind<'a> {
    Simulated(&'a SimBackend),
    #[cfg(all(feature = "arm-live-egress", not(test)))]
    Live {
        backend: &'a ProdBackend,
        selection: LiveSelectionProof,
    },
}

#[derive(Debug)]
#[cfg(all(feature = "arm-live-egress", not(test)))]
struct LiveSelectionProof {
    private: (),
}

impl<'a> RuntimeBackend<'a> {
    /// The default, network-incapable runtime selection.
    pub const fn simulated(backend: &'a SimBackend) -> Self {
        Self { inner: RuntimeBackendKind::Simulated(backend) }
    }

    /// Reduce the explicit startup flag exactly once to a non-`Clone` selection.
    #[cfg(all(feature = "arm-live-egress", not(test)))]
    pub fn from_explicit_flag(
        explicit_live: bool,
        simulated: &'a SimBackend,
        live: &'a ProdBackend,
    ) -> Self {
        if explicit_live {
            Self {
                inner: RuntimeBackendKind::Live {
                    backend: live,
                    selection: LiveSelectionProof { private: () },
                },
            }
        } else {
            Self::simulated(simulated)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveLockSnapshot {
    explicit_live: bool,
    signed_receipt_fresh: bool,
    kill_clear: bool,
    funded_balance: Result<Option<U256>, ProviderError>,
    signed_cap: U256,
}

impl LiveLockSnapshot {
    #[cfg(all(feature = "arm-live-egress", not(test)))]
    fn from_live_selection(
        _selection: LiveSelectionProof,
        freshness: FreshnessProof,
        funded_balance: Result<Option<U256>, ProviderError>,
        signed_cap: U256,
    ) -> Self {
        Self {
            explicit_live: true,
            signed_receipt_fresh: freshness.signed_receipt_fresh(),
            kill_clear: freshness.kill_clear(),
            funded_balance,
            signed_cap,
        }
    }
}

fn evaluate_live_locks(snapshot: &LiveLockSnapshot) -> Result<(), LiveLockClosed> {
    if !snapshot.explicit_live {
        return Err(LiveLockClosed::ExplicitLiveSelection);
    }
    if !snapshot.signed_receipt_fresh {
        return Err(LiveLockClosed::SignedReceipt);
    }
    if !snapshot.kill_clear {
        return Err(LiveLockClosed::KillAnchor);
    }
    let balance = snapshot
        .funded_balance
        .as_ref()
        .map_err(|_| LiveLockClosed::FundsUnavailable)?
        .ok_or(LiveLockClosed::FundedAccountAbsent)?;
    if balance > snapshot.signed_cap {
        return Err(LiveLockClosed::FundsCapExceeded);
    }
    Ok(())
}

/// The single gated submission entry point.
pub fn send_gated(
    attempt: SubmissionAttempt,
    fresh: &FreshnessSources<'_>,
    backend: RuntimeBackend<'_>,
) -> SubmitOutcome {
    let (egress, freshness) = match attempt {
        SubmissionAttempt::Initial(paired) => {
            let Some(freshness) = fresh.revalidate(&paired.bindings, &paired.id) else {
                return SubmitOutcome::NoEgress;
            };
            (
                RawEgress {
                    plan: EgressPlan::Initial {
                        inclusion: paired.inclusion,
                        attribution: paired.attribution,
                        bindings: paired.bindings,
                        id: paired.id,
                        expected_inclusion_hash: paired.expected_inclusion_hash,
                    },
                },
                freshness,
            )
        }
        SubmissionAttempt::AttributionRetry(token) => {
            let Some(freshness) = fresh.revalidate(&token.bindings, &token.id) else {
                return SubmitOutcome::NoEgress;
            };
            (
                RawEgress {
                    plan: EgressPlan::AttributionOnly {
                        attribution: token.attribution,
                        bindings: token.bindings,
                        id: token.id,
                        inclusion_receipt_hash: token.inclusion_receipt_hash,
                    },
                },
                freshness,
            )
        }
    };

    match backend.inner {
        RuntimeBackendKind::Simulated(simulated) => {
            let _ = freshness;
            simulated.execute(BackendPermit::Simulated(SimEgressPermit { private: () }), egress)
        }
        #[cfg(all(feature = "arm-live-egress", not(test)))]
        RuntimeBackendKind::Live { backend, selection } => {
            let snapshot = LiveLockSnapshot::from_live_selection(
                selection,
                freshness,
                fresh.code_hash.native_balance_at_latest_committed(super::custody::FUNDED_WALLET),
                fresh.armed.hot_wallet_cap_wei(),
            );
            if let Err(reason) = evaluate_live_locks(&snapshot) {
                return SubmitOutcome::LiveLocksClosed(reason);
            }
            backend.execute(BackendPermit::Live(LiveEgressPermit { private: () }), egress)
        }
    }
}

// -- pure response mapping (testable offline) ---------------------------------

/// Map an inclusion-channel HTTP response to the returned tx hash. `None` (→
/// inclusion failure) on any non-2xx status, malformed JSON, a JSON-RPC `error`
/// object, or a missing/invalid `result` hash. Pure: no network, unit-tested.
#[cfg(any(test, feature = "arm-live-egress"))]
pub(crate) fn parse_inclusion_result(status: u16, body: &[u8]) -> Option<B256> {
    if !(200..300).contains(&status) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value.get("error").is_some() {
        return None;
    }
    value.get("result")?.as_str()?.parse::<B256>().ok()
}

/// Whether an attribution-channel HTTP response is a SUCCESS. `false` (→
/// attribution failure, retry token preserved) on any non-2xx status (401/403/429/
/// 5xx …), malformed JSON, a JSON-RPC `error` object, or a `result` that is not a
/// NON-EMPTY STRING (the bundle/submission id). `null` / `""` / boolean / number /
/// object results are all failures. Pure: no network, unit-tested.
///
/// NOTE: the exact Blink success schema beyond "`result` is a non-empty string id"
/// is a G7/Blink-BD residual to be confirmed at B6.
#[cfg(any(test, feature = "arm-live-egress"))]
pub(crate) fn attribution_response_ok(status: u16, body: &[u8]) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    if value.get("error").is_some() {
        return false;
    }
    // `result` must be a non-empty string id; every other shape is a failure.
    value.get("result").and_then(serde_json::Value::as_str).is_some_and(|id| !id.is_empty())
}

// -- production backend (SOLE real egress site) -------------------------------

/// The real two-channel egress backend. `execute` is the ONLY `reqwest` call site
/// in the crate and compiles solely under `arm-live-egress` + `not(test)`. It holds
/// a SINGLE reused blocking client (connection pool + pinned timeout). Per the
/// [`send_gated`] isolation contract, it is invoked only from the B5 dedicated
/// bounded egress worker.
#[cfg(all(feature = "arm-live-egress", not(test)))]
#[derive(Debug)]
pub struct ProdBackend {
    client: reqwest::blocking::Client,
}

#[cfg(all(feature = "arm-live-egress", not(test)))]
impl sealed::Sealed for ProdBackend {}

#[cfg(all(feature = "arm-live-egress", not(test)))]
impl ProdBackend {
    /// Build the backend with a single reused blocking client. Constructed once by
    /// the B5 dedicated-egress worker.
    pub fn new() -> Result<Self, ()> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_millis(2_000))
            .build()
            .map_err(|_| ())?;
        Ok(Self { client })
    }

    /// POST `body` to `url`; return `(http_status, response_bytes)`.
    fn post(&self, url: &str, body: &[u8]) -> Result<(u16, Vec<u8>), ()> {
        let response = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .map_err(|_| ())?;
        let status = response.status().as_u16();
        let bytes = response.bytes().map_err(|_| ())?;
        Ok((status, bytes.to_vec()))
    }

    /// Send the inclusion channel; return the node-returned tx hash on success (a
    /// non-2xx / RPC-error / bad-result response yields `None`).
    fn send_inclusion(&self, spec: &RequestSpec) -> Option<B256> {
        let (status, body) = self.post(spec.endpoint(), spec.body()).ok()?;
        parse_inclusion_result(status, &body)
    }

    /// Send the attribution channel to the Blink auction. Per the MUST-USE contract
    /// the API key is a URL PATH segment (`{BLINK_AUCTION_HOST}{key}`), NOT a header.
    /// The key-bearing URL is held in a `Zeroizing` buffer. A non-2xx status, a
    /// JSON-RPC `error`, or a missing `result` all count as attribution failure.
    fn send_attribution(&self, spec: &RequestSpec) -> bool {
        let Ok(credential) = super::custody::BlinkCredential::load() else {
            return false;
        };
        let Ok(key) = core::str::from_utf8(credential.expose()) else {
            return false;
        };
        let url = zeroize::Zeroizing::new(format!("{}{}", spec.endpoint(), key));
        let Ok((status, body)) = self.post(&url, spec.body()) else {
            return false;
        };
        attribution_response_ok(status, &body)
    }
}

#[cfg(all(feature = "arm-live-egress", not(test)))]
impl RawBackend for ProdBackend {
    fn execute(&self, permit: BackendPermit, egress: RawEgress) -> SubmitOutcome {
        if !matches!(permit, BackendPermit::Live(LiveEgressPermit { private: () })) {
            return SubmitOutcome::NoEgress;
        }
        execute_live_sequence(
            egress,
            |request| self.send_inclusion(request),
            |request| self.send_attribution(request),
        )
    }
}

// -- shared live execution sequence -------------------------------------------

/// Module-private sequencing shared by production and tests. Channel operations
/// are supplied only by this module; no caller can inject behavior.
fn execute_live_sequence<I, A>(
    egress: RawEgress,
    mut inclusion_result: I,
    mut attribution_result: A,
) -> SubmitOutcome
where
    I: FnMut(&RequestSpec) -> Option<B256>,
    A: FnMut(&RequestSpec) -> bool,
{
    match egress.into_plan() {
        EgressPlan::Initial { inclusion, attribution, bindings, id, expected_inclusion_hash } => {
            match inclusion_result(&inclusion) {
                Some(hash) if hash == expected_inclusion_hash => {}
                _ => return SubmitOutcome::InclusionFailed,
            }
            if !attribution_result(&attribution) {
                return SubmitOutcome::InclusionSentAttributionFailed(AttributionRetryToken::new(
                    attribution,
                    bindings,
                    id,
                    expected_inclusion_hash,
                ));
            }
            SubmitOutcome::LiveComplete
        }
        EgressPlan::AttributionOnly { attribution, bindings, id, inclusion_receipt_hash } => {
            if !attribution_result(&attribution) {
                return SubmitOutcome::InclusionSentAttributionFailed(AttributionRetryToken::new(
                    attribution,
                    bindings,
                    id,
                    inclusion_receipt_hash,
                ));
            }
            SubmitOutcome::LiveComplete
        }
    }
}

#[cfg(test)]
fn validate_live_lock_fixture(fixture: &serde_json::Value) -> Result<LiveLockSnapshot, String> {
    let object = fixture.as_object().ok_or_else(|| "fixture must be an object".to_owned())?;
    let actual = object.keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>();
    let expected = std::collections::BTreeSet::from([
        "explicit_live",
        "funded_account",
        "kill_state",
        "receipt_state",
        "signed_cap_wei",
    ]);
    if actual != expected {
        return Err(format!("fixture keys must be exactly {expected:?}, got {actual:?}"));
    }

    let explicit_live = object["explicit_live"]
        .as_bool()
        .ok_or_else(|| "explicit_live must be boolean".to_owned())?;
    let signed_receipt_fresh = match object["receipt_state"]
        .as_str()
        .ok_or_else(|| "receipt_state must be string".to_owned())?
    {
        "fresh" => true,
        "absent" | "mismatched" | "not-yet-valid" | "expired" => false,
        other => return Err(format!("unclassified receipt_state `{other}`")),
    };
    let kill_clear = match object["kill_state"]
        .as_str()
        .ok_or_else(|| "kill_state must be string".to_owned())?
    {
        "clear" => true,
        "unknown" | "engaged" | "poisoned" => false,
        other => return Err(format!("unclassified kill_state `{other}`")),
    };
    let funded_balance = match object["funded_account"]
        .as_object()
        .ok_or_else(|| "funded_account must be object".to_owned())?
    {
        account
            if account.len() == 2
                && account.get("status").and_then(serde_json::Value::as_str) == Some("present") =>
        {
            let balance = account
                .get("balance_wei")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "present account balance_wei must be decimal string".to_owned())?;
            Ok(Some(
                U256::from_str_radix(balance, 10)
                    .map_err(|_| "balance_wei must be U256 decimal".to_owned())?,
            ))
        }
        account
            if account.len() == 1
                && account.get("status").and_then(serde_json::Value::as_str) == Some("absent") =>
        {
            Ok(None)
        }
        account
            if account.len() == 1
                && account.get("status").and_then(serde_json::Value::as_str) == Some("error") =>
        {
            Err(ProviderError::Unavailable("fixture".to_owned()))
        }
        _ => return Err("unclassified funded_account".to_owned()),
    };
    let cap = object["signed_cap_wei"]
        .as_str()
        .ok_or_else(|| "signed_cap_wei must be decimal string".to_owned())?;
    let signed_cap = U256::from_str_radix(cap, 10)
        .map_err(|_| "signed_cap_wei must be U256 decimal".to_owned())?;

    Ok(LiveLockSnapshot {
        explicit_live,
        signed_receipt_fresh,
        kill_clear,
        funded_balance,
        signed_cap,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::B256;
    use base_mev_trader::{ArmedCriteria, CampaignId, StoreIdentity};

    use super::*;
    use crate::arm::custody::HotWalletKey;
    use crate::arm::proofs::SubmitSuppressionClear;
    use crate::arm::request::Channel;
    use crate::arm::suppression::{SuppressionEpochStore, SuppressionFileStore};
    use crate::arm::testkit as tk;
    use crate::arm::witness::{
        AuthorizedCandidate, CheckedCandidate, DeploymentIdentity, FreshnessSources,
        PairedSubmission,
    };

    fn campaign() -> CampaignId {
        CampaignId::new([0x0Au8; 32])
    }

    struct Harness {
        _dir: tk::TempDir,
        armed: ArmedCriteria,
        drawdown: tk::FakeDrawdown,
        suppression_file: SuppressionFileStore,
        supp_path: std::path::PathBuf,
        epoch_store: SuppressionEpochStore,
        provider: tk::FakeProvider,
        dep_id: tk::FakeDeploymentIdentity,
        clock: tk::FakeClock,
        sink: Arc<super::super::ArmedFailSink>,
    }

    impl Harness {
        fn fresh(&self) -> FreshnessSources<'_> {
            FreshnessSources::new(
                &self.armed,
                &self.drawdown,
                &self.suppression_file,
                &self.epoch_store,
                &self.provider,
                &self.dep_id,
                &self.clock,
                Arc::clone(&self.sink),
            )
        }
    }

    /// Build a full positive harness + a ready `PairedSubmission` and the exact
    /// expected inclusion/attribution bodies + inclusion hash.
    fn build() -> (Harness, PairedSubmission, Vec<u8>, Vec<u8>, B256) {
        let now = 1_000;
        let dir = tk::TempDir::new("tx");
        let code_hash = B256::repeat_byte(0x33);
        let store = StoreIdentity::new([0x55u8; 32]);
        let (vtx, victim) = tk::validated_tx(tk::EXECUTOR);
        let cand = CheckedCandidate::new(vtx, campaign());
        // Mint the claim, then re-anchor its identity by using the same StoreIdentity
        // in the deployment. victim_claim bootstraps its own store; align identities.
        let (claim, claim_store) = tk::victim_claim(&dir.path, victim, campaign());
        let provider = tk::FakeProvider { code_hash, block: 100, fail: false };
        let deploy = tk::deployment(
            &provider,
            tk::EXECUTOR,
            code_hash,
            B256::repeat_byte(1),
            B256::repeat_byte(2),
            claim_store,
        );
        let _ = store;
        let g7 = tk::g7(campaign(), now + 100, now);
        // Live window opens at `now - 500` and closes at `now + 100`, so the egress
        // window_start re-check is exercisable by rewinding the clock below 500.
        let live = tk::live_windowed(campaign(), now - 500, now + 100, now);
        let supp_path = tk::write_suppression_file(&dir.path, 5, false);
        let suppression_file = SuppressionFileStore::new(&supp_path);
        let epoch_store = tk::epoch_store(&dir.path);
        let sup = SubmitSuppressionClear::read(&suppression_file, &epoch_store).unwrap();
        let authorized =
            AuthorizedCandidate::issue_checked(true, sup, g7, claim, live, deploy, cand).unwrap();
        let (key, address) = tk::hot_wallet_key();
        let wpath = tk::write_hot_wallet(&dir.path, &key);
        let sink = tk::sink(&dir.path);
        let signed = authorized
            .load_and_sign_with(&sink, || HotWalletKey::load_from(&wpath, address))
            .unwrap();
        let expected_hash = signed.raw_tx_hash();
        let paired = PairedSubmission::assemble(signed);
        let incl_body = paired.inclusion.body().to_vec();
        let attr_body = paired.attribution.body().to_vec();
        let harness = Harness {
            _dir: dir,
            armed: tk::unarmed_criteria(),
            drawdown: tk::complete_zero_drawdown(),
            suppression_file,
            supp_path,
            epoch_store,
            provider,
            dep_id: tk::FakeDeploymentIdentity(Some(DeploymentIdentity {
                binary_digest: B256::repeat_byte(1),
                deployment_digest: B256::repeat_byte(2),
                r9_store_identity: claim_store,
            })),
            clock: tk::FakeClock(Some(now)),
            sink,
        };
        (harness, paired, incl_body, attr_body, expected_hash)
    }

    fn raw_initial(paired: PairedSubmission) -> RawEgress {
        RawEgress {
            plan: EgressPlan::Initial {
                inclusion: paired.inclusion,
                attribution: paired.attribution,
                bindings: paired.bindings,
                id: paired.id,
                expected_inclusion_hash: paired.expected_inclusion_hash,
            },
        }
    }

    fn raw_retry(token: AttributionRetryToken) -> RawEgress {
        RawEgress {
            plan: EgressPlan::AttributionOnly {
                attribution: token.attribution,
                bindings: token.bindings,
                id: token.id,
                inclusion_receipt_hash: token.inclusion_receipt_hash,
            },
        }
    }

    fn simulate(attempt: SubmissionAttempt, fresh: &FreshnessSources<'_>) -> SubmitOutcome {
        let backend = SimBackend;
        send_gated(attempt, fresh, RuntimeBackend::simulated(&backend))
    }

    #[test]
    fn production_simulation_records_exact_wire_without_live_completion() {
        let (harness, paired, incl_body, attr_body, expected) = build();
        let outcome =
            simulate(SubmissionAttempt::Initial(paired), &harness.fresh().with_forced_gate(true));
        let record = match outcome {
            SubmitOutcome::Simulated(record) => record,
            other => panic!("expected simulation record, got {other:?}"),
        };
        assert_eq!(record.attempt(), SimulationAttempt::Initial);
        assert_eq!(record.inclusion_receipt_hash(), expected);
        let requests = record.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].channel(), Channel::Inclusion);
        assert_eq!(requests[0].method(), "eth_sendRawTransaction");
        assert_eq!(requests[0].endpoint(), "http://127.0.0.1:8545");
        assert_eq!(requests[0].body(), incl_body);
        assert_eq!(requests[1].channel(), Channel::Attribution);
        assert_eq!(requests[1].method(), "eth_sendBundle");
        assert_eq!(requests[1].endpoint(), "https://baseauction.blinklabs.xyz/v1/");
        assert_eq!(requests[1].body(), attr_body);
        let body: serde_json::Value = serde_json::from_slice(&attr_body).unwrap();
        assert_eq!(body["params"][0]["bidWei"], "0");
    }

    #[test]
    fn shared_live_sequence_maps_inclusion_failures() {
        let (_harness, paired, _i, _a, expected) = build();
        let failed = execute_live_sequence(raw_initial(paired), |_| None, |_| true);
        assert!(matches!(failed, SubmitOutcome::InclusionFailed));

        let (_harness, paired, _i, _a, _expected) = build();
        let mismatched =
            execute_live_sequence(raw_initial(paired), |_| Some(B256::repeat_byte(0xAB)), |_| true);
        assert!(matches!(mismatched, SubmitOutcome::InclusionFailed));
        assert_ne!(expected, B256::repeat_byte(0xAB));
    }

    #[test]
    fn shared_live_sequence_preserves_attribution_retry_shape() {
        let (_harness, paired, _i, _a, expected) = build();
        let outcome = execute_live_sequence(raw_initial(paired), |_| Some(expected), |_| false);
        let token = match outcome {
            SubmitOutcome::InclusionSentAttributionFailed(token) => token,
            other => panic!("expected partial failure, got {other:?}"),
        };
        assert_eq!(token.inclusion_receipt_hash(), expected);

        let retry = execute_live_sequence(
            raw_retry(token),
            |_| panic!("attribution retry must not send inclusion"),
            |_| true,
        );
        assert!(matches!(retry, SubmitOutcome::LiveComplete));
    }

    #[test]
    fn send_gated_retry_revalidates_and_simulates_attribution_only() {
        let (harness, paired, _i, _a, expected) = build();
        let initial = execute_live_sequence(raw_initial(paired), |_| Some(expected), |_| false);
        let token = match initial {
            SubmitOutcome::InclusionSentAttributionFailed(token) => token,
            other => panic!("expected retry token, got {other:?}"),
        };
        let outcome = simulate(
            SubmissionAttempt::AttributionRetry(token),
            &harness.fresh().with_forced_gate(true),
        );
        let record = match outcome {
            SubmitOutcome::Simulated(record) => record,
            other => panic!("expected simulated retry, got {other:?}"),
        };
        assert_eq!(record.attempt(), SimulationAttempt::AttributionRetry);
        assert_eq!(record.inclusion_receipt_hash(), expected);
        assert_eq!(record.requests().len(), 1);
        assert_eq!(record.requests()[0].channel(), Channel::Attribution);

        let (mut stale_harness, stale_paired, _i, _a, stale_expected) = build();
        let stale_initial =
            execute_live_sequence(raw_initial(stale_paired), |_| Some(stale_expected), |_| false);
        let stale_token = match stale_initial {
            SubmitOutcome::InclusionSentAttributionFailed(token) => token,
            other => panic!("expected stale retry token, got {other:?}"),
        };
        stale_harness.clock = tk::FakeClock(Some(1_100));
        let stale = simulate(
            SubmissionAttempt::AttributionRetry(stale_token),
            &stale_harness.fresh().with_forced_gate(true),
        );
        assert!(matches!(stale, SubmitOutcome::NoEgress));
    }

    #[test]
    fn closed_gate_yields_no_egress() {
        let (harness, paired, _i, _a, _expected) = build();
        let outcome = simulate(SubmissionAttempt::Initial(paired), &harness.fresh());
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn poisoned_sink_yields_no_egress() {
        let (harness, paired, _i, _a, _expected) = build();
        harness.sink.latch(base_mev_trader::KillReason::KeyOrSignatureFailure);
        let outcome =
            simulate(SubmissionAttempt::Initial(paired), &harness.fresh().with_forced_gate(true));
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn stale_deployment_identity_yields_no_egress() {
        let (mut harness, paired, _i, _a, _expected) = build();
        harness.dep_id = tk::FakeDeploymentIdentity(Some(DeploymentIdentity {
            binary_digest: B256::repeat_byte(0x99),
            deployment_digest: B256::repeat_byte(2),
            r9_store_identity: StoreIdentity::new([0x55u8; 32]),
        }));
        let outcome =
            simulate(SubmissionAttempt::Initial(paired), &harness.fresh().with_forced_gate(true));
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn suppression_lock_at_egress_yields_no_egress() {
        let (harness, paired, _i, _a, _expected) = build();
        let mut lock = harness.supp_path.clone().into_os_string();
        lock.push(".lock");
        std::fs::write(std::path::PathBuf::from(lock), b"").unwrap();
        let outcome =
            simulate(SubmissionAttempt::Initial(paired), &harness.fresh().with_forced_gate(true));
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn closed_live_windows_yield_no_egress() {
        for now in [400, 1_100] {
            let (mut harness, paired, _i, _a, _expected) = build();
            harness.clock = tk::FakeClock(Some(now));
            let outcome = simulate(
                SubmissionAttempt::Initial(paired),
                &harness.fresh().with_forced_gate(true),
            );
            assert!(matches!(outcome, SubmitOutcome::NoEgress), "clock {now}");
        }
    }

    fn live_fixture() -> serde_json::Value {
        serde_json::json!({
            "explicit_live": true,
            "receipt_state": "fresh",
            "kill_state": "clear",
            "funded_account": {"status": "present", "balance_wei": "100"},
            "signed_cap_wei": "100"
        })
    }

    fn assert_lock_mutant(
        original: &serde_json::Value,
        mutant: &serde_json::Value,
        name: &str,
        expected: LiveLockClosed,
    ) {
        assert_ne!(mutant, original, "{name} patch did not change fixture");
        let snapshot = validate_live_lock_fixture(mutant).expect("classified mutant");
        assert_eq!(evaluate_live_locks(&snapshot), Err(expected));
        eprintln!("{name}: RED");
    }

    #[test]
    fn lock_l0_exact_cap_is_green() {
        let fixture = live_fixture();
        let snapshot = validate_live_lock_fixture(&fixture).unwrap();
        assert_eq!(evaluate_live_locks(&snapshot), Ok(()));
        eprintln!("L0: GREEN");
    }

    #[test]
    fn lock_l0z_present_zero_balance_is_green() {
        let original = live_fixture();
        let mut fixture = original.clone();
        fixture["funded_account"]["balance_wei"] = serde_json::json!("0");
        assert_ne!(fixture, original, "L0z patch did not change fixture");
        let snapshot = validate_live_lock_fixture(&fixture).unwrap();
        assert_eq!(evaluate_live_locks(&snapshot), Ok(()));
        eprintln!("L0z: GREEN");
    }

    #[test]
    fn lock_l1_free_bool_is_red() {
        let original = live_fixture();
        let mut mutant = original.clone();
        mutant["explicit_live"] = serde_json::json!(false);
        assert_lock_mutant(&original, &mutant, "L1", LiveLockClosed::ExplicitLiveSelection);
    }

    #[test]
    fn lock_l2_receipt_failures_are_red() {
        for state in ["absent", "mismatched", "not-yet-valid", "expired"] {
            let original = live_fixture();
            let mut mutant = original.clone();
            mutant["receipt_state"] = serde_json::json!(state);
            assert_lock_mutant(&original, &mutant, "L2", LiveLockClosed::SignedReceipt);
        }
    }

    #[test]
    fn lock_l3_kill_failures_are_red() {
        for state in ["unknown", "engaged", "poisoned"] {
            let original = live_fixture();
            let mut mutant = original.clone();
            mutant["kill_state"] = serde_json::json!(state);
            assert_lock_mutant(&original, &mutant, "L3", LiveLockClosed::KillAnchor);
        }
    }

    #[test]
    fn lock_l4a_absent_account_is_red() {
        let original = live_fixture();
        let mut mutant = original.clone();
        mutant["funded_account"] = serde_json::json!({"status": "absent"});
        assert_lock_mutant(&original, &mutant, "L4a", LiveLockClosed::FundedAccountAbsent);
    }

    #[test]
    fn lock_l4b_authority_error_is_red() {
        let original = live_fixture();
        let mut mutant = original.clone();
        mutant["funded_account"] = serde_json::json!({"status": "error"});
        assert_lock_mutant(&original, &mutant, "L4b", LiveLockClosed::FundsUnavailable);
    }

    #[test]
    fn lock_l4c_over_cap_is_red() {
        let original = live_fixture();
        let mut mutant = original.clone();
        mutant["funded_account"]["balance_wei"] = serde_json::json!("101");
        assert_lock_mutant(&original, &mutant, "L4c", LiveLockClosed::FundsCapExceeded);
    }

    fn validate_freshness_proof_source(transport: &str, witness: &str) -> Result<(), String> {
        let proof_call = ["let Some(freshness) = fresh.", "revalidate"].concat();
        if transport.matches(&proof_call).count() != 2 {
            return Err("initial and retry branches must each mint freshness proof".to_owned());
        }
        let constructor = transport
            .split_once("fn from_live_selection")
            .and_then(|(_, rest)| rest.split_once("fn evaluate_live_locks"))
            .map(|(body, _)| body)
            .ok_or_else(|| "live snapshot constructor missing".to_owned())?;
        if !constructor.contains("signed_receipt_fresh: freshness.signed_receipt_fresh()")
            || !constructor.contains("kill_clear: freshness.kill_clear()")
            || constructor.contains("signed_receipt_fresh: true")
            || constructor.contains("kill_clear: true")
        {
            return Err("L2/L3 are not derived from freshness proof".to_owned());
        }
        if transport.contains(&["live_", "requested"].concat()) {
            return Err("live-only freshness bypass present".to_owned());
        }
        if witness
            .matches("Some(FreshnessProof {\n            signed_receipt: SignedReceiptFresh")
            .count()
            != 1
        {
            return Err("freshness proof must have one revalidate mint site".to_owned());
        }
        Ok(())
    }

    #[test]
    fn freshness_f0_green_f1_bypass_f2_receipt_literal_f3_kill_literal_red() {
        let transport = include_str!("transport.rs");
        let witness = include_str!("witness.rs");
        validate_freshness_proof_source(transport, witness).expect("F0 source must be sealed");
        eprintln!("F0: GREEN");

        let proof_call = ["let Some(freshness) = fresh.", "revalidate"].concat();
        let bypass =
            ["if !live_", "requested { let Some(freshness) = fresh.", "revalidate"].concat();
        let mutant = transport.replacen(&proof_call, &bypass, 1);
        assert_ne!(mutant, transport, "F1 patch did not change source");
        assert!(validate_freshness_proof_source(&mutant, witness).is_err());
        eprintln!("F1: RED");

        let mutant = transport.replacen(
            "signed_receipt_fresh: freshness.signed_receipt_fresh()",
            "signed_receipt_fresh: true",
            1,
        );
        assert_ne!(mutant, transport, "F2 patch did not change source");
        assert!(validate_freshness_proof_source(&mutant, witness).is_err());
        eprintln!("F2: RED");

        let mutant =
            transport.replacen("kill_clear: freshness.kill_clear()", "kill_clear: true", 1);
        assert_ne!(mutant, transport, "F3 patch did not change source");
        assert!(validate_freshness_proof_source(&mutant, witness).is_err());
        eprintln!("F3: RED");
    }
    fn validate_live_sequence_source(source: &str) -> Result<(), String> {
        let definition = ["fn execute_live", "_sequence<I, A>("].concat();
        let definition_index = source
            .find(&definition)
            .ok_or_else(|| "shared live sequence definition missing".to_owned())?;
        let prefix = &source[definition_index.saturating_sub(160)..definition_index];
        if prefix.contains("#[cfg(test)]") || prefix.ends_with("pub ") {
            return Err(
                "shared live sequence must be private and compile in both configs".to_owned()
            );
        }

        let production = source
            .split_once("impl RawBackend for ProdBackend")
            .and_then(|(_, rest)| rest.split_once("// -- shared live execution sequence"))
            .map(|(body, _)| body)
            .ok_or_else(|| "ProdBackend execute body missing".to_owned())?;
        let call = ["execute_live", "_sequence("].concat();
        if production.matches(&call).count() != 1 {
            return Err("ProdBackend must delegate exactly once to shared sequence".to_owned());
        }
        if production.contains("match egress.into_plan()") {
            return Err("ProdBackend contains duplicated sequencing".to_owned());
        }
        if source.matches(&call).count() != 7 {
            return Err("shared sequence call-site count changed".to_owned());
        }

        let helper = source[definition_index..]
            .split_once("#[cfg(test)]\nfn validate_live_lock_fixture")
            .map(|(body, _)| body)
            .ok_or_else(|| "shared sequence boundary missing".to_owned())?;
        if helper.matches("match egress.into_plan()").count() != 1 {
            return Err("shared sequence must own the sole plan match".to_owned());
        }
        Ok(())
    }

    #[test]
    fn sequence_c0_green_c1_delegation_red_c2_test_gate_red() {
        let source = include_str!("transport.rs");
        validate_live_sequence_source(source).expect("C0 source must be sealed");
        eprintln!("C0: GREEN");

        let production_call = ["execute_live", "_sequence(\n            egress,"].concat();
        let mutant = source.replacen(
            &production_call,
            "execute_live_sequence_copy(\n            egress,",
            1,
        );
        assert_ne!(mutant, source, "C1 patch did not change source");
        assert!(validate_live_sequence_source(&mutant).is_err());
        eprintln!("C1: RED");

        let definition = ["fn execute_live", "_sequence<I, A>("].concat();
        let mutant =
            source.replacen(&definition, "#[cfg(test)]\nfn execute_live_sequence<I, A>(", 1);
        assert_ne!(mutant, source, "C2 patch did not change source");
        assert!(validate_live_sequence_source(&mutant).is_err());
        eprintln!("C2: RED");
    }

    #[test]
    fn inclusion_response_mapping() {
        let hash = B256::repeat_byte(0xAB);
        let ok_body = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"{hash:#x}\"}}");
        assert_eq!(parse_inclusion_result(200, ok_body.as_bytes()), Some(hash));
        for status in [401u16, 403, 429, 500, 502, 503] {
            assert!(parse_inclusion_result(status, ok_body.as_bytes()).is_none());
        }
        let error = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000}}";
        assert!(parse_inclusion_result(200, error).is_none());
        assert!(parse_inclusion_result(200, b"not json").is_none());
    }

    #[test]
    fn attribution_response_mapping() {
        let ok_body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"0xbundlehash\"}";
        assert!(attribution_response_ok(200, ok_body));
        for status in [401u16, 403, 429, 500, 502, 503] {
            assert!(!attribution_response_ok(status, ok_body));
        }
        for wrong in [
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":true}",
            "{\"jsonrpc\":\"2.0\"}",
            "not json",
        ] {
            assert!(!attribution_response_ok(200, wrong.as_bytes()));
        }
    }
}
