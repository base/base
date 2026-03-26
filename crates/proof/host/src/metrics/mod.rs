//! Metrics for the proof host.
//!
//! All metric names are prefixed with `base_proof_host_`.
//!
//! ## Counters
//!
//! | Name | Labels | Description |
//! |------|--------|-------------|
//! | `base_proof_host_requests_total` | `mode` | Total proof requests received |
//! | `base_proof_host_requests_result_total` | `outcome` | Proof request outcomes (incl. `dropped`) |
//! | `base_proof_host_hint_requests_total` | `hint_type` | Hint requests by type |
//! | `base_proof_host_hint_errors_total` | `hint_type` | Hint errors by type |
//! | `base_proof_host_kv_cold_lookups_total` | | KV lookups that missed the cache (resolved via hint fetch) |
//! | `base_proof_host_preimage_accesses_total` | | Total preimage accesses |
//! | `base_proof_host_offline_misses_total` | | Offline backend key misses |
//!
//! ## Gauges
//!
//! | Name | Labels | Description |
//! |------|--------|-------------|
//! | `base_proof_host_in_flight_proofs` | | Currently in-flight proof requests |
//! | `base_proof_host_preimage_count` | | Preimage count from last witness build |
//!
//! ## Histograms
//!
//! | Name | Labels | Description |
//! |------|--------|-------------|
//! | `base_proof_host_proof_duration_seconds` | | End-to-end proof generation duration |
//! | `base_proof_host_witness_build_duration_seconds` | | Witness build duration |
//! | `base_proof_host_prover_duration_seconds` | | Backend prover duration |
//! | `base_proof_host_hint_duration_seconds` | `hint_type` | Hint processing duration by type |
//! | `base_proof_host_replay_duration_seconds` | | Client replay (prologue+execute+validate) duration |

/// Container for metrics.
#[derive(Debug, Clone)]
pub struct Metrics;

/// RAII timer that records elapsed duration to a histogram metric on drop.
///
/// Call [`.stop()`](Self::stop) to record early; otherwise the duration is
/// recorded when the guard is dropped.
#[cfg(feature = "metrics")]
pub struct DropTimer {
    histogram: metrics::Histogram,
    start: std::time::Instant,
    stopped: bool,
}

/// No-op timer used when the `metrics` feature is disabled.
#[cfg(not(feature = "metrics"))]
pub struct DropTimer;

#[cfg(feature = "metrics")]
impl std::fmt::Debug for DropTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropTimer").finish_non_exhaustive()
    }
}

#[cfg(not(feature = "metrics"))]
impl std::fmt::Debug for DropTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropTimer").finish()
    }
}

#[cfg(feature = "metrics")]
impl DropTimer {
    /// Creates a new timer. Use the [`timed!`] macro instead.
    #[inline]
    pub fn new(histogram: metrics::Histogram) -> Self {
        Self { histogram, start: std::time::Instant::now(), stopped: false }
    }

    /// Stops the timer, recording the elapsed duration to the histogram.
    ///
    /// Subsequent calls and the drop are no-ops.
    #[inline]
    pub fn stop(&mut self) {
        if !self.stopped {
            self.histogram.record(self.start.elapsed().as_secs_f64());
            self.stopped = true;
        }
    }
}

#[cfg(not(feature = "metrics"))]
impl DropTimer {
    /// Creates a no-op timer.
    #[inline]
    pub const fn new() -> Self {
        Self
    }

    /// No-op.
    #[inline]
    pub fn stop(&mut self) {}
}

#[cfg(feature = "metrics")]
impl Drop for DropTimer {
    fn drop(&mut self) {
        self.stop();
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
#[cfg(feature = "metrics")]
pub struct ProofGuard {
    gauge: &'static str,
    counter: &'static str,
    outcome: &'static str,
}

/// No-op guard used when the `metrics` feature is disabled.
#[cfg(not(feature = "metrics"))]
pub struct ProofGuard;

#[cfg(feature = "metrics")]
impl std::fmt::Debug for ProofGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofGuard").finish_non_exhaustive()
    }
}

#[cfg(not(feature = "metrics"))]
impl std::fmt::Debug for ProofGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofGuard").finish()
    }
}

#[cfg(feature = "metrics")]
impl ProofGuard {
    /// Creates a new guard. Prefer the [`proof_guard!`] macro.
    #[inline]
    pub fn new(gauge: &'static str, counter: &'static str) -> Self {
        base_metrics::inc!(gauge, gauge);
        Self { gauge, counter, outcome: Metrics::OUTCOME_DROPPED }
    }

    /// Overrides the outcome that will be recorded when this guard drops.
    #[inline]
    pub const fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}

#[cfg(not(feature = "metrics"))]
impl ProofGuard {
    /// Creates a no-op guard.
    #[inline]
    pub const fn new() -> Self {
        Self
    }

    /// No-op.
    #[inline]
    pub fn set_outcome(&mut self, _outcome: &'static str) {}
}

#[cfg(feature = "metrics")]
impl Drop for ProofGuard {
    fn drop(&mut self) {
        let gauge = self.gauge;
        let counter = self.counter;
        let outcome = self.outcome;
        base_metrics::dec!(gauge, gauge);
        base_metrics::inc!(counter, counter, Metrics::LABEL_OUTCOME => outcome);
    }
}

/// Creates a [`DropTimer`] that records elapsed duration to a histogram.
///
/// # Examples
///
/// ```ignore
/// // Drop-based: records when `_timer` goes out of scope.
/// let _timer = timed!(Metrics::PROOF_DURATION_SECONDS);
///
/// // Explicit stop: records immediately, drop is a no-op.
/// let mut timer = timed!(Metrics::WITNESS_BUILD_DURATION_SECONDS);
/// let result = do_work().await;
/// timer.stop();
///
/// // With labels:
/// let _timer = timed!(Metrics::HINT_DURATION_SECONDS, Metrics::LABEL_HINT_TYPE => label);
/// ```
macro_rules! timed {
    ($metric:expr $(, $label_key:expr => $label_value:expr)*$(,)?) => {{
        #[cfg(feature = "metrics")]
        { $crate::DropTimer::new(metrics::histogram!($metric $(, $label_key => $label_value)*)) }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = ($metric, $($label_key, $label_value,)*);
            $crate::DropTimer::new()
        }
    }};
}

pub(crate) use timed;

/// Creates a [`ProofGuard`] that tracks an in-flight proof.
///
/// # Examples
///
/// ```ignore
/// let mut guard = proof_guard!(Metrics::IN_FLIGHT_PROOFS, Metrics::REQUESTS_RESULT_TOTAL);
/// let result = do_work().await;
/// guard.set_outcome(Metrics::OUTCOME_SUCCESS);
/// // gauge decremented and outcome counter incremented on drop
/// ```
macro_rules! proof_guard {
    ($gauge:expr, $counter:expr) => {{
        #[cfg(feature = "metrics")]
        {
            $crate::ProofGuard::new($gauge, $counter)
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = ($gauge, $counter);
            $crate::ProofGuard::new()
        }
    }};
}

pub(crate) use proof_guard;

impl Metrics {
    // ---- Counters ----

    /// Total proof requests received, labeled by `mode`.
    pub const REQUESTS_TOTAL: &str = "base_proof_host_requests_total";

    /// Proof request outcomes, labeled by `outcome`
    /// (`success/witness_error/prove_error/dropped`).
    pub const REQUESTS_RESULT_TOTAL: &str = "base_proof_host_requests_result_total";

    /// Hint requests by type, labeled by `hint_type`.
    pub const HINT_REQUESTS_TOTAL: &str = "base_proof_host_hint_requests_total";

    /// Hint processing errors by type, labeled by `hint_type`.
    pub const HINT_ERRORS_TOTAL: &str = "base_proof_host_hint_errors_total";

    /// KV lookups that missed the cache and required hint fetching.
    pub const KV_COLD_LOOKUPS_TOTAL: &str = "base_proof_host_kv_cold_lookups_total";

    /// Total preimage accesses through the recording oracle.
    pub const PREIMAGE_ACCESSES_TOTAL: &str = "base_proof_host_preimage_accesses_total";

    /// Offline backend key-not-found events.
    pub const OFFLINE_MISSES_TOTAL: &str = "base_proof_host_offline_misses_total";

    // ---- Gauges ----

    /// Currently in-flight proof requests.
    pub const IN_FLIGHT_PROOFS: &str = "base_proof_host_in_flight_proofs";

    /// Number of preimages captured in the last witness build.
    pub const PREIMAGE_COUNT: &str = "base_proof_host_preimage_count";

    // ---- Histograms ----

    /// End-to-end proof generation duration in seconds.
    pub const PROOF_DURATION_SECONDS: &str = "base_proof_host_proof_duration_seconds";

    /// Witness build duration in seconds.
    pub const WITNESS_BUILD_DURATION_SECONDS: &str =
        "base_proof_host_witness_build_duration_seconds";

    /// Backend prover duration in seconds.
    pub const PROVER_DURATION_SECONDS: &str = "base_proof_host_prover_duration_seconds";

    /// Per-hint-type processing duration in seconds, labeled by `hint_type`.
    pub const HINT_DURATION_SECONDS: &str = "base_proof_host_hint_duration_seconds";

    /// Client replay duration in seconds (prologue + execute + validate).
    pub const REPLAY_DURATION_SECONDS: &str = "base_proof_host_replay_duration_seconds";

    // ---- Label keys ----

    /// Label key for the operating mode.
    pub const LABEL_MODE: &str = "mode";

    /// Label key for outcome classification.
    pub const LABEL_OUTCOME: &str = "outcome";

    /// Label key for the hint type.
    pub const LABEL_HINT_TYPE: &str = "hint_type";

    // ---- Label values ----

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
}

impl Metrics {
    /// Registers metric descriptions and initializes all counters/gauges to zero
    /// so they appear in dashboards immediately.
    ///
    /// No-op when the `metrics` feature is disabled.
    #[cfg(feature = "metrics")]
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    /// No-op when the `metrics` feature is disabled.
    #[cfg(not(feature = "metrics"))]
    pub fn init() {}

    #[cfg(feature = "metrics")]
    fn describe() {
        metrics::describe_counter!(Self::REQUESTS_TOTAL, "Total proof requests received");
        metrics::describe_counter!(Self::REQUESTS_RESULT_TOTAL, "Proof request outcomes by result");
        metrics::describe_counter!(Self::HINT_REQUESTS_TOTAL, "Hint requests by type");
        metrics::describe_counter!(Self::HINT_ERRORS_TOTAL, "Hint processing errors by type");
        metrics::describe_counter!(
            Self::KV_COLD_LOOKUPS_TOTAL,
            "KV lookups that missed the cache and required hint fetching"
        );
        metrics::describe_counter!(
            Self::PREIMAGE_ACCESSES_TOTAL,
            "Total preimage accesses through the recording oracle"
        );
        metrics::describe_counter!(
            Self::OFFLINE_MISSES_TOTAL,
            "Offline backend key-not-found events"
        );
        metrics::describe_gauge!(Self::IN_FLIGHT_PROOFS, "Currently in-flight proof requests");
        metrics::describe_gauge!(
            Self::PREIMAGE_COUNT,
            "Number of preimages captured in the last witness build"
        );

        metrics::describe_histogram!(
            Self::PROOF_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "End-to-end proof generation duration"
        );
        metrics::describe_histogram!(
            Self::WITNESS_BUILD_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "Witness build duration"
        );
        metrics::describe_histogram!(
            Self::PROVER_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "Backend prover duration"
        );
        metrics::describe_histogram!(
            Self::HINT_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "Per-hint-type processing duration"
        );
        metrics::describe_histogram!(
            Self::REPLAY_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "Client replay duration"
        );
    }

    #[cfg(feature = "metrics")]
    fn zero() {
        base_metrics::set!(gauge, Self::IN_FLIGHT_PROOFS, 0);
        base_metrics::set!(gauge, Self::PREIMAGE_COUNT, 0);

        base_metrics::set!(counter, Self::REQUESTS_TOTAL, Self::LABEL_MODE, Self::MODE_ONLINE, 0);

        base_metrics::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_SUCCESS,
            0
        );
        base_metrics::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_WITNESS_ERROR,
            0
        );
        base_metrics::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_PROVE_ERROR,
            0
        );
        base_metrics::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_DROPPED,
            0
        );

        base_metrics::set!(counter, Self::KV_COLD_LOOKUPS_TOTAL, 0);

        base_metrics::set!(counter, Self::PREIMAGE_ACCESSES_TOTAL, 0);
        base_metrics::set!(counter, Self::OFFLINE_MISSES_TOTAL, 0);
    }
}
