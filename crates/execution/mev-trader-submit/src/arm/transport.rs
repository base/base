//! Two-channel transport. [`send_gated`] is the SINGLE entry point: it consumes a
//! [`SubmissionAttempt`] by value, re-validates the ENTIRE freshness conjunction at
//! the egress moment, and only then mints a linear [`RawEgress`] permit and hands
//! it to a [`RawBackend`]. [`ProdBackend::execute`] is the SOLE real network call
//! site in the whole crate, and it only compiles under `arm-live-egress` +
//! `not(test)`. Tests drive [`FakeBackend`] (no network) through the identical
//! `send_gated` path.

use alloy_primitives::B256;

use super::request::RequestSpec;
use super::witness::{FreshnessSources, PairedSubmission, ProofBindings, ValidatedExecutionIdentity};

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

/// The typed outcome of a gated submission.
// `InclusionSentAttributionFailed` carries the full retry permit; the outcome is
// produced once per submission and matched immediately, so boxing the permit would
// add an allocation on the submit path for no benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SubmitOutcome {
    /// The egress-moment re-validation failed (or the process was poisoned): no
    /// bytes left the process.
    NoEgress,
    /// The inclusion channel failed (nothing to retry).
    InclusionFailed,
    /// Inclusion landed but attribution failed — carries the retry permit. The
    /// inclusion is NEVER re-sent; only attribution may be retried.
    InclusionSentAttributionFailed(AttributionRetryToken),
    /// Both channels succeeded.
    Complete,
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
    // Reachable (via the pub `RawBackend` supertrait bound) but deliberately
    // UNNAMEABLE outside this module — that is exactly the seal: no external crate
    // can name or implement it, so `ProdBackend`/`FakeBackend` are the only egress
    // backends. The `unnameable_types` warning is the intended property, not a bug.
    #[allow(unnameable_types)]
    /// Sealed supertrait: only this module can name it, so no crate outside can
    /// implement [`super::RawBackend`]. The ONLY implementors are [`super::ProdBackend`]
    /// (live-egress + non-test) and [`super::FakeBackend`] (test).
    pub trait Sealed {}
}

/// The low-level egress backend, SEALED so only [`ProdBackend`] (the sole socket
/// opener, live-egress + non-test) and [`FakeBackend`] (test) can implement it —
/// even a future crate that links this one cannot introduce another egress path.
pub trait RawBackend: sealed::Sealed {
    /// Perform the permitted egress and report the typed outcome.
    fn execute(&self, egress: RawEgress) -> SubmitOutcome;
}

/// The single gated submission entry point. Re-validates freshness at the egress
/// moment and, only on success, mints a [`RawEgress`] permit for `backend`.
///
/// ## Node-thread isolation contract (B5 wiring)
/// The real backend ([`ProdBackend`]) performs SYNCHRONOUS blocking HTTP (up to the
/// per-request timeout) for up to two channels. It MUST therefore be invoked ONLY
/// from a dedicated, bounded OS worker thread that the B5 node-linkage wiring spawns
/// off any node-critical / async-runtime worker — never inline on a Tokio worker or
/// the ExEx/consensus path. This is a wiring-time contract (there is no in-tier
/// entrypoint yet); `send_gated` itself is pure/blocking and imposes no runtime.
pub fn send_gated<B: RawBackend>(
    attempt: SubmissionAttempt,
    fresh: &FreshnessSources<'_>,
    backend: &B,
) -> SubmitOutcome {
    match attempt {
        SubmissionAttempt::Initial(paired) => {
            if !fresh.revalidate(&paired.bindings, &paired.id) {
                return SubmitOutcome::NoEgress;
            }
            let egress = RawEgress {
                plan: EgressPlan::Initial {
                    inclusion: paired.inclusion,
                    attribution: paired.attribution,
                    bindings: paired.bindings,
                    id: paired.id,
                    expected_inclusion_hash: paired.expected_inclusion_hash,
                },
            };
            backend.execute(egress)
        }
        SubmissionAttempt::AttributionRetry(token) => {
            if !fresh.revalidate(&token.bindings, &token.id) {
                return SubmitOutcome::NoEgress;
            }
            let egress = RawEgress {
                plan: EgressPlan::AttributionOnly {
                    attribution: token.attribution,
                    bindings: token.bindings,
                    id: token.id,
                    inclusion_receipt_hash: token.inclusion_receipt_hash,
                },
            };
            backend.execute(egress)
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
    fn execute(&self, egress: RawEgress) -> SubmitOutcome {
        match egress.into_plan() {
            EgressPlan::Initial {
                inclusion,
                attribution,
                bindings,
                id,
                expected_inclusion_hash,
            } => {
                // Inclusion channel: require the node-returned hash to equal ours.
                match self.send_inclusion(&inclusion) {
                    Some(hash) if hash == expected_inclusion_hash => {}
                    _ => return SubmitOutcome::InclusionFailed,
                }
                // Attribution channel (inclusion already landed → preserve the token).
                if !self.send_attribution(&attribution) {
                    return SubmitOutcome::InclusionSentAttributionFailed(
                        AttributionRetryToken::new(attribution, bindings, id, expected_inclusion_hash),
                    );
                }
                SubmitOutcome::Complete
            }
            EgressPlan::AttributionOnly { attribution, bindings, id, inclusion_receipt_hash } => {
                if !self.send_attribution(&attribution) {
                    return SubmitOutcome::InclusionSentAttributionFailed(
                        AttributionRetryToken::new(attribution, bindings, id, inclusion_receipt_hash),
                    );
                }
                SubmitOutcome::Complete
            }
        }
    }
}

// -- offline test backend -----------------------------------------------------

/// A recorded channel send (channel/endpoint/body) captured by [`FakeBackend`].
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct RecordedSend {
    pub(crate) channel: super::request::Channel,
    pub(crate) endpoint: &'static str,
    pub(crate) method: &'static str,
    pub(crate) body: Vec<u8>,
}

/// A per-channel simulated backend result.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum FakeResult {
    /// The node/host returned this hash (success iff it equals the expected hash).
    Hash(B256),
    /// The host returned no/invalid result (transport or RPC error).
    Error,
}

/// An offline backend: records every channel body and returns configured results.
/// Opens NO socket.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FakeBackend {
    inclusion: FakeResult,
    attribution: FakeResult,
    sent: std::cell::RefCell<Vec<RecordedSend>>,
}

#[cfg(test)]
impl FakeBackend {
    pub(crate) fn new(inclusion: FakeResult, attribution: FakeResult) -> Self {
        Self { inclusion, attribution, sent: std::cell::RefCell::new(Vec::new()) }
    }

    pub(crate) fn sent(&self) -> Vec<RecordedSend> {
        self.sent.borrow().clone()
    }

    fn record(&self, spec: &RequestSpec) {
        self.sent.borrow_mut().push(RecordedSend {
            channel: spec.channel(),
            endpoint: spec.endpoint(),
            method: spec.method(),
            body: spec.body().to_vec(),
        });
    }
}

#[cfg(test)]
impl sealed::Sealed for FakeBackend {}

#[cfg(test)]
impl RawBackend for FakeBackend {
    fn execute(&self, egress: RawEgress) -> SubmitOutcome {
        match egress.into_plan() {
            EgressPlan::Initial {
                inclusion,
                attribution,
                bindings,
                id,
                expected_inclusion_hash,
            } => {
                self.record(&inclusion);
                match self.inclusion {
                    FakeResult::Hash(hash) if hash == expected_inclusion_hash => {}
                    _ => return SubmitOutcome::InclusionFailed,
                }
                self.record(&attribution);
                match self.attribution {
                    FakeResult::Hash(_) => SubmitOutcome::Complete,
                    FakeResult::Error => SubmitOutcome::InclusionSentAttributionFailed(
                        AttributionRetryToken::new(attribution, bindings, id, expected_inclusion_hash),
                    ),
                }
            }
            EgressPlan::AttributionOnly { attribution, bindings, id, inclusion_receipt_hash } => {
                self.record(&attribution);
                match self.attribution {
                    FakeResult::Hash(_) => SubmitOutcome::Complete,
                    FakeResult::Error => SubmitOutcome::InclusionSentAttributionFailed(
                        AttributionRetryToken::new(attribution, bindings, id, inclusion_receipt_hash),
                    ),
                }
            }
        }
    }
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

    #[test]
    fn positive_complete_with_exact_wire() {
        let (harness, paired, incl_body, attr_body, expected) = build();
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::Complete));
        let sent = backend.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].channel, Channel::Inclusion);
        assert_eq!(sent[0].method, "eth_sendRawTransaction");
        assert_eq!(sent[0].endpoint, "http://127.0.0.1:8545");
        assert_eq!(sent[0].body, incl_body);
        assert_eq!(sent[1].channel, Channel::Attribution);
        assert_eq!(sent[1].method, "eth_sendBundle");
        assert_eq!(sent[1].endpoint, "https://baseauction.blinklabs.xyz/v1/");
        assert_eq!(sent[1].body, attr_body);
        // The attribution body carries bidWei "0".
        let body: serde_json::Value = serde_json::from_slice(&attr_body).unwrap();
        assert_eq!(body["params"][0]["bidWei"], "0");
    }

    #[test]
    fn inclusion_error_is_inclusion_failed() {
        let (harness, paired, _i, _a, expected) = build();
        let backend = FakeBackend::new(FakeResult::Error, FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::InclusionFailed));
        // Only the inclusion channel was attempted.
        assert_eq!(backend.sent().len(), 1);
    }

    #[test]
    fn returned_hash_mismatch_is_inclusion_failed() {
        let (harness, paired, _i, _a, _expected) = build();
        let backend = FakeBackend::new(
            FakeResult::Hash(B256::repeat_byte(0xAB)),
            FakeResult::Hash(B256::repeat_byte(0xAB)),
        );
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::InclusionFailed));
    }

    #[test]
    fn attribution_failure_yields_retry_then_completes() {
        let (harness, paired, _i, _a, expected) = build();
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Error);
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        let token = match outcome {
            SubmitOutcome::InclusionSentAttributionFailed(token) => token,
            other => panic!("expected partial failure, got {other:?}"),
        };
        assert_eq!(token.inclusion_receipt_hash(), expected);
        // Retry attribution only; this time it succeeds. Full fresh re-validation runs.
        let backend2 = FakeBackend::new(FakeResult::Error, FakeResult::Hash(expected));
        let outcome2 = send_gated(
            SubmissionAttempt::AttributionRetry(token),
            &harness.fresh().with_forced_gate(true),
            &backend2,
        );
        assert!(matches!(outcome2, SubmitOutcome::Complete));
        // Only the attribution channel was re-sent on retry.
        let sent = backend2.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel, Channel::Attribution);
    }

    #[test]
    fn closed_gate_yields_no_egress() {
        let (harness, paired, _i, _a, expected) = build();
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        // Real (unarmed) gate, no forced open -> NoEgress, nothing sent.
        let outcome = send_gated(SubmissionAttempt::Initial(paired), &harness.fresh(), &backend);
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
        assert!(backend.sent().is_empty());
    }

    #[test]
    fn poisoned_sink_yields_no_egress() {
        let (harness, paired, _i, _a, expected) = build();
        // Poison the sink.
        harness.sink.latch(base_mev_trader::KillReason::KeyOrSignatureFailure);
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
        assert!(backend.sent().is_empty());
    }

    #[test]
    fn stale_deployment_identity_yields_no_egress() {
        let (mut harness, paired, _i, _a, expected) = build();
        // Change the live deployment identity so it no longer matches the bindings.
        harness.dep_id = tk::FakeDeploymentIdentity(Some(DeploymentIdentity {
            binary_digest: B256::repeat_byte(0x99),
            deployment_digest: B256::repeat_byte(2),
            r9_store_identity: StoreIdentity::new([0x55u8; 32]),
        }));
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn suppression_lock_at_egress_yields_no_egress() {
        // C2: a writer lock appearing AFTER the initial proof but BEFORE egress
        // (mid-write / stale crash) must fail-close the egress re-validation.
        let (harness, paired, _i, _a, expected) = build();
        let mut lock = harness.supp_path.clone().into_os_string();
        lock.push(".lock");
        std::fs::write(std::path::PathBuf::from(lock), b"").unwrap();
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
        assert!(backend.sent().is_empty());
    }

    #[test]
    fn live_window_not_yet_open_yields_no_egress() {
        // M2: the live window is [now-500, now+100). Rewind the clock to 400 (< the
        // window_start of 500) so ONLY the window_start re-check fails (g7 has no
        // window and is not expired at 400). Egress must fail-close.
        let (mut harness, paired, _i, _a, expected) = build();
        harness.clock = tk::FakeClock(Some(400));
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn live_window_closed_at_expiry_yields_no_egress() {
        // Clock at/after expiry (1100) -> window closed -> NoEgress.
        let (mut harness, paired, _i, _a, expected) = build();
        harness.clock = tk::FakeClock(Some(1_100));
        let backend = FakeBackend::new(FakeResult::Hash(expected), FakeResult::Hash(expected));
        let outcome = send_gated(
            SubmissionAttempt::Initial(paired),
            &harness.fresh().with_forced_gate(true),
            &backend,
        );
        assert!(matches!(outcome, SubmitOutcome::NoEgress));
    }

    #[test]
    fn inclusion_response_mapping() {
        // C3: pure HTTP-response → result mapping.
        let hash = B256::repeat_byte(0xAB);
        let ok_body = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"{hash:#x}\"}}");
        assert_eq!(parse_inclusion_result(200, ok_body.as_bytes()), Some(hash));
        // Non-2xx statuses fail.
        for status in [401u16, 403, 429, 500, 502, 503] {
            assert!(parse_inclusion_result(status, ok_body.as_bytes()).is_none());
        }
        // JSON-RPC error object -> fail (even with 200).
        let err_body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"x\"}}";
        assert!(parse_inclusion_result(200, err_body).is_none());
        // Missing / malformed result.
        assert!(parse_inclusion_result(200, b"{\"jsonrpc\":\"2.0\"}").is_none());
        assert!(parse_inclusion_result(200, b"not json").is_none());
    }

    #[test]
    fn attribution_response_mapping() {
        // C3: only a NON-EMPTY STRING result is success.
        let ok_body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"0xbundlehash\"}";
        assert!(attribution_response_ok(200, ok_body));
        // Non-2xx statuses fail even with a valid body.
        for status in [401u16, 403, 429, 500, 502, 503] {
            assert!(!attribution_response_ok(status, ok_body), "status {status} wrongly ok");
        }
        // JSON-RPC error object fails.
        let err_body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"unauthorized\"}}";
        assert!(!attribution_response_ok(200, err_body));
        // M2: result of the wrong shape / empty MUST be attribution failure.
        for wrong in [
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":true}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":123}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"id\":\"x\"}}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":[\"x\"]}",
            "{\"jsonrpc\":\"2.0\"}",
            "not json",
        ] {
            assert!(!attribution_response_ok(200, wrong.as_bytes()), "wrongly ok: {wrong}");
        }
    }
}
