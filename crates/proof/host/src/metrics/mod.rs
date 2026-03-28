//! Metrics for the proof host.

base_metrics::define_metrics! {
    base_proof_host

    #[describe("Total proof requests received")]
    #[label(mode)]
    requests_total: counter,

    #[describe("Proof request outcomes by result")]
    #[label(outcome)]
    requests_result_total: counter,

    #[describe("Hint requests by type")]
    #[label(hint_type)]
    hint_requests_total: counter,

    #[describe("Hint processing errors by type")]
    #[label(hint_type)]
    hint_errors_total: counter,

    #[describe("KV lookups that missed the cache and required hint fetching")]
    kv_cold_lookups_total: counter,

    #[describe("Total preimage accesses through the recording oracle")]
    preimage_accesses_total: counter,

    #[describe("Offline backend key-not-found events")]
    offline_misses_total: counter,

    #[describe("Currently in-flight proof requests")]
    in_flight_proofs: gauge,

    #[describe("Number of preimages captured in the last witness build")]
    preimage_count: gauge,

    #[describe("End-to-end proof generation duration")]
    proof_duration_seconds: histogram,

    #[describe("Witness build duration")]
    witness_build_duration_seconds: histogram,

    #[describe("Backend prover duration")]
    prover_duration_seconds: histogram,

    #[describe("Per-hint-type processing duration")]
    #[label(hint_type)]
    hint_duration_seconds: histogram,

    #[describe("Client replay duration")]
    replay_duration_seconds: histogram,
}

impl Metrics {
    /// Online operating mode.
    pub const MODE_ONLINE: &str = "online";

    /// Successful proof outcome.
    pub const OUTCOME_SUCCESS: &str = "success";

    /// Witness generation error outcome.
    pub const OUTCOME_WITNESS_ERROR: &str = "witness_error";

    /// Backend proving error outcome.
    pub const OUTCOME_PROVE_ERROR: &str = "prove_error";

    /// Future was cancelled (dropped) before completion.
    pub const OUTCOME_DROPPED: &str = "dropped";

    /// Registers metric descriptions and initializes all counters/gauges to zero
    /// so they appear in dashboards immediately.
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    fn zero() {
        Self::in_flight_proofs().set(0);
        Self::preimage_count().set(0);
        Self::requests_total(Self::MODE_ONLINE).absolute(0);
        Self::requests_result_total(Self::OUTCOME_SUCCESS).absolute(0);
        Self::requests_result_total(Self::OUTCOME_WITNESS_ERROR).absolute(0);
        Self::requests_result_total(Self::OUTCOME_PROVE_ERROR).absolute(0);
        Self::requests_result_total(Self::OUTCOME_DROPPED).absolute(0);
        Self::kv_cold_lookups_total().absolute(0);
        Self::preimage_accesses_total().absolute(0);
        Self::offline_misses_total().absolute(0);
    }
}

/// RAII guard for in-flight proof tracking.
///
/// Increments a gauge on creation and decrements it on drop. Records the
/// outcome to a counter on drop — defaulting to [`Metrics::OUTCOME_DROPPED`]
/// so that cancelled futures are always accounted for.
///
/// Use [`set_outcome`](Self::set_outcome) on the success/error path to
/// override the default before the guard drops.
///
/// Prefer the [`proof_guard!`] macro to construct this type.
#[cfg(feature = "metrics")]
pub struct ProofGuard {
    _inflight: base_metrics::InflightCounter,
    outcome: &'static str,
}

#[cfg(feature = "metrics")]
impl std::fmt::Debug for ProofGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofGuard").finish_non_exhaustive()
    }
}

#[cfg(feature = "metrics")]
impl ProofGuard {
    /// Starts tracking an in-flight proof request.
    #[inline]
    pub(crate) fn track_inflight() -> Self {
        Self {
            _inflight: base_metrics::InflightCounter::new(Metrics::in_flight_proofs()),
            outcome: Metrics::OUTCOME_DROPPED,
        }
    }

    /// Overrides the outcome that will be recorded when this guard drops.
    #[inline]
    pub const fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}

#[cfg(feature = "metrics")]
impl Drop for ProofGuard {
    fn drop(&mut self) {
        Metrics::requests_result_total(self.outcome).increment(1);
    }
}

/// No-op guard used when the `metrics` feature is disabled.
#[derive(Debug)]
pub struct NoopProofGuard;

impl NoopProofGuard {
    /// No-op.
    #[inline(always)]
    pub const fn set_outcome(&mut self, _outcome: &'static str) {}
}

/// Creates a [`ProofGuard`] that tracks an in-flight proof, or a
/// [`NoopProofGuard`] when the `metrics` feature is disabled.
///
/// # Examples
///
/// ```ignore
/// let mut guard = proof_guard!();
/// let result = do_work().await;
/// guard.set_outcome(Metrics::OUTCOME_SUCCESS);
/// // gauge decremented and outcome counter incremented on drop
/// ```
macro_rules! proof_guard {
    () => {{
        #[cfg(feature = "metrics")]
        {
            $crate::ProofGuard::track_inflight()
        }
        #[cfg(not(feature = "metrics"))]
        {
            $crate::NoopProofGuard
        }
    }};
}

pub(crate) use proof_guard;
