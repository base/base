//! Metrics for the proof host.
//!
//! All metric names are prefixed with `base_proof_host_`.
//!
//! ## Counters
//!
//! | Name | Labels | Description |
//! |------|--------|-------------|
//! | `base_proof_host_requests_total` | `mode` | Total proof requests received |
//! | `base_proof_host_requests_result_total` | `outcome` | Proof request outcomes |
//! | `base_proof_host_hint_requests_total` | `hint_type` | Hint requests by type |
//! | `base_proof_host_hint_errors_total` | `hint_type` | Hint errors by type |
//! | `base_proof_host_kv_lookups_total` | `result` | KV store lookups by hit/miss |
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
//! | `base_proof_host_provider_connect_duration_seconds` | `provider` | RPC provider connection time |
//! | `base_proof_host_witness_size_bytes` | | Witness size in bytes |
//! | `base_proof_host_replay_duration_seconds` | | Client replay (prologue+execute+validate) duration |
//! | `base_proof_host_rpc_payload_size_bytes` | `hint_type` | RPC response payload size by hint type |

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
#[macro_export]
macro_rules! timed {
    ($metric:expr $(, $label_key:expr => $label_value:expr)*$(,)?) => {{
        // Suppress unused-variable warnings for `$metric` and label arguments.
        #[cfg(not(feature = "metrics"))]
        { let _ = ($metric, $($label_key, $label_value,)*); }
        #[cfg(feature = "metrics")]
        { $crate::DropTimer::new(metrics::histogram!($metric $(, $label_key => $label_value)*)) }
        #[cfg(not(feature = "metrics"))]
        { $crate::DropTimer::new() }
    }};
}

pub(crate) use timed;

impl Metrics {
    // ---- Counters ----

    /// Total proof requests received, labeled by `mode` (online/offline).
    pub const REQUESTS_TOTAL: &str = "base_proof_host_requests_total";

    /// Proof request outcomes, labeled by `outcome`
    /// (success/rpc_error/witness_error/prove_error/timeout).
    pub const REQUESTS_RESULT_TOTAL: &str = "base_proof_host_requests_result_total";

    /// Hint requests by type, labeled by `hint_type`.
    pub const HINT_REQUESTS_TOTAL: &str = "base_proof_host_hint_requests_total";

    /// Hint processing errors by type, labeled by `hint_type`.
    pub const HINT_ERRORS_TOTAL: &str = "base_proof_host_hint_errors_total";

    /// KV store lookup results, labeled by `result` (hit/miss).
    pub const KV_LOOKUPS_TOTAL: &str = "base_proof_host_kv_lookups_total";

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

    /// RPC provider connection time in seconds, labeled by `provider` (l1/beacon/l2).
    pub const PROVIDER_CONNECT_DURATION_SECONDS: &str =
        "base_proof_host_provider_connect_duration_seconds";

    /// Witness size in bytes.
    pub const WITNESS_SIZE_BYTES: &str = "base_proof_host_witness_size_bytes";

    /// Client replay duration in seconds (prologue + execute + validate).
    pub const REPLAY_DURATION_SECONDS: &str = "base_proof_host_replay_duration_seconds";

    /// RPC response payload size in bytes, labeled by `hint_type`.
    pub const RPC_PAYLOAD_SIZE_BYTES: &str = "base_proof_host_rpc_payload_size_bytes";

    // ---- Label keys ----

    /// Label key for the operating mode.
    pub const LABEL_MODE: &str = "mode";

    /// Label key for outcome classification.
    pub const LABEL_OUTCOME: &str = "outcome";

    /// Label key for the hint type.
    pub const LABEL_HINT_TYPE: &str = "hint_type";

    /// Label key for KV lookup result.
    pub const LABEL_RESULT: &str = "result";

    /// Label key for the RPC provider name.
    pub const LABEL_PROVIDER: &str = "provider";

    // ---- Label values ----

    /// Online operating mode.
    pub const MODE_ONLINE: &str = "online";

    /// Offline operating mode.
    pub const MODE_OFFLINE: &str = "offline";

    /// Successful proof outcome.
    pub const OUTCOME_SUCCESS: &str = "success";

    /// RPC error outcome.
    pub const OUTCOME_RPC_ERROR: &str = "rpc_error";

    /// Witness generation error outcome.
    pub const OUTCOME_WITNESS_ERROR: &str = "witness_error";

    /// Backend proving error outcome.
    pub const OUTCOME_PROVE_ERROR: &str = "prove_error";

    /// KV cache hit.
    pub const RESULT_HIT: &str = "hit";

    /// KV cache miss.
    pub const RESULT_MISS: &str = "miss";

    /// L1 provider.
    pub const PROVIDER_L1: &str = "l1";

    /// Beacon provider.
    pub const PROVIDER_BEACON: &str = "beacon";

    /// L2 provider.
    pub const PROVIDER_L2: &str = "l2";
}

impl Metrics {
    /// Registers metric descriptions and initializes all counters/gauges to zero
    /// so they appear in dashboards immediately.
    #[cfg(feature = "metrics")]
    pub fn init() {
        Self::describe();
        Self::zero();
    }

    #[cfg(feature = "metrics")]
    fn describe() {
        metrics::describe_counter!(Self::REQUESTS_TOTAL, "Total proof requests received");
        metrics::describe_counter!(Self::REQUESTS_RESULT_TOTAL, "Proof request outcomes by result");
        metrics::describe_counter!(Self::HINT_REQUESTS_TOTAL, "Hint requests by type");
        metrics::describe_counter!(Self::HINT_ERRORS_TOTAL, "Hint processing errors by type");
        metrics::describe_counter!(Self::KV_LOOKUPS_TOTAL, "KV store lookups by hit or miss");
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
            Self::PROVIDER_CONNECT_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "RPC provider connection time"
        );
        metrics::describe_histogram!(
            Self::WITNESS_SIZE_BYTES,
            metrics::Unit::Bytes,
            "Witness size"
        );
        metrics::describe_histogram!(
            Self::REPLAY_DURATION_SECONDS,
            metrics::Unit::Seconds,
            "Client replay duration"
        );
        metrics::describe_histogram!(
            Self::RPC_PAYLOAD_SIZE_BYTES,
            metrics::Unit::Bytes,
            "RPC response payload size by hint type"
        );
    }

    #[cfg(feature = "metrics")]
    fn zero() {
        base_macros::set!(gauge, Self::IN_FLIGHT_PROOFS, 0);
        base_macros::set!(gauge, Self::PREIMAGE_COUNT, 0);

        base_macros::set!(counter, Self::REQUESTS_TOTAL, Self::LABEL_MODE, Self::MODE_ONLINE, 0);
        base_macros::set!(counter, Self::REQUESTS_TOTAL, Self::LABEL_MODE, Self::MODE_OFFLINE, 0);

        base_macros::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_SUCCESS,
            0
        );
        base_macros::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_RPC_ERROR,
            0
        );
        base_macros::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_WITNESS_ERROR,
            0
        );
        base_macros::set!(
            counter,
            Self::REQUESTS_RESULT_TOTAL,
            Self::LABEL_OUTCOME,
            Self::OUTCOME_PROVE_ERROR,
            0
        );

        base_macros::set!(counter, Self::KV_LOOKUPS_TOTAL, Self::LABEL_RESULT, Self::RESULT_HIT, 0);
        base_macros::set!(
            counter,
            Self::KV_LOOKUPS_TOTAL,
            Self::LABEL_RESULT,
            Self::RESULT_MISS,
            0
        );
    }
}
