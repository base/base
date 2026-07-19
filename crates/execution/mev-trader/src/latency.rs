//! Deterministic latency measurement and reporting.

use std::time::Instant;

/// Required untimed warmup repetitions per fixture.
pub const LATENCY_WARMUP_RUNS: usize = 10;
/// Required timed repetitions per fixture.
pub const LATENCY_TIMED_RUNS: usize = 100;
/// Exclusive completed-latency threshold in nanoseconds.
pub const LATENCY_THRESHOLD_NS: u64 = 50_000_000;

/// Invalid terminal accounting or completed-sample shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyError {
    /// A required denominator was zero.
    ZeroDenominator,
    /// Terminal counters do not satisfy the conservation equations.
    InvalidAccounting,
    /// Completed sample count differs from the completed counter.
    SampleCountMismatch,
    /// Terminal reporting observed work still in flight.
    NotDrained,
}

impl std::fmt::Display for LatencyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDenominator => formatter.write_str("latency denominator is zero"),
            Self::InvalidAccounting => formatter.write_str("latency accounting is invalid"),
            Self::SampleCountMismatch => {
                formatter.write_str("completed latency sample count mismatch")
            }
            Self::NotDrained => formatter.write_str("latency report is not terminally drained"),
        }
    }
}

impl std::error::Error for LatencyError {}

/// Terminal frame accounting with explicit pre/post-admission drops.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LatencyAccounting {
    /// Every frame observed before admission.
    pub received: u64,
    /// Frames rejected before admission.
    pub pre_admission_dropped: u64,
    /// Frames accepted for bounded processing.
    pub admitted: u64,
    /// Admitted frames completed with a terminal output decision.
    pub completed: u64,
    /// Admitted frames dropped after admission.
    pub post_admission_dropped: u64,
    /// Partial outputs, required to remain zero in production.
    pub truncated: u64,
    /// Work not yet terminally accounted.
    pub in_flight: u64,
}

impl LatencyAccounting {
    /// Returns the named total drop count.
    pub const fn dropped(&self) -> u64 {
        self.pre_admission_dropped + self.post_admission_dropped
    }

    /// Validates both conservation equations using checked arithmetic.
    pub fn validate(&self) -> Result<(), LatencyError> {
        let received = self
            .pre_admission_dropped
            .checked_add(self.admitted)
            .ok_or(LatencyError::InvalidAccounting)?;
        let admitted = self
            .completed
            .checked_add(self.post_admission_dropped)
            .and_then(|value| value.checked_add(self.truncated))
            .and_then(|value| value.checked_add(self.in_flight))
            .ok_or(LatencyError::InvalidAccounting)?;
        if self.received != received || self.admitted != admitted {
            return Err(LatencyError::InvalidAccounting);
        }
        if self.in_flight != 0 {
            return Err(LatencyError::NotDrained);
        }
        Ok(())
    }
}

/// Mutable deterministic counter used by offline fixture harnesses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LatencyRecorder {
    accounting: LatencyAccounting,
    completed_ns: Vec<u64>,
}

impl LatencyRecorder {
    /// Records one pre-admission rejection.
    pub fn record_pre_admission_drop(&mut self) -> Result<(), LatencyError> {
        self.accounting.received =
            self.accounting.received.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.accounting.pre_admission_dropped = self
            .accounting
            .pre_admission_dropped
            .checked_add(1)
            .ok_or(LatencyError::InvalidAccounting)?;
        Ok(())
    }

    /// Records one admission and starts its in-flight lifetime.
    pub fn record_admission(&mut self) -> Result<(), LatencyError> {
        self.accounting.received =
            self.accounting.received.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.accounting.admitted =
            self.accounting.admitted.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.accounting.in_flight =
            self.accounting.in_flight.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        Ok(())
    }

    /// Records one completed admitted frame and its end-to-end latency.
    pub fn record_completion(&mut self, latency_ns: u64) -> Result<(), LatencyError> {
        self.accounting.in_flight =
            self.accounting.in_flight.checked_sub(1).ok_or(LatencyError::InvalidAccounting)?;
        self.accounting.completed =
            self.accounting.completed.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.completed_ns.push(latency_ns);
        Ok(())
    }

    /// Records one post-admission zero-output drop.
    pub fn record_post_admission_drop(&mut self) -> Result<(), LatencyError> {
        self.accounting.in_flight =
            self.accounting.in_flight.checked_sub(1).ok_or(LatencyError::InvalidAccounting)?;
        self.accounting.post_admission_dropped = self
            .accounting
            .post_admission_dropped
            .checked_add(1)
            .ok_or(LatencyError::InvalidAccounting)?;
        Ok(())
    }

    /// Records a partial result only for a negative seal canary.
    pub fn record_truncation_canary(&mut self) -> Result<(), LatencyError> {
        self.accounting.in_flight =
            self.accounting.in_flight.checked_sub(1).ok_or(LatencyError::InvalidAccounting)?;
        self.accounting.truncated =
            self.accounting.truncated.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        Ok(())
    }

    /// Builds one terminal report after exact drain.
    pub fn finish(self) -> Result<LatencyReport, LatencyError> {
        LatencyReport::from_terminal(self.accounting, self.completed_ns)
    }

    /// Returns current counters without producing a report.
    pub const fn accounting(&self) -> LatencyAccounting {
        self.accounting
    }
}

/// Completed-only nearest-rank quantiles and honest terminal accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyReport {
    /// Every frame observed before admission.
    pub received: u64,
    /// Frames accepted for bounded processing.
    pub admitted: u64,
    /// Admitted frames with a completed terminal decision.
    pub completed: u64,
    /// Pre- plus post-admission drops.
    pub dropped: u64,
    /// Partial outputs, required to remain zero.
    pub truncated: u64,
    /// Terminal in-flight count, required to remain zero.
    pub in_flight: u64,
    /// Completed-only nearest-rank p50.
    pub completed_p50_ns: u64,
    /// Completed-only nearest-rank p95.
    pub completed_p95_ns: u64,
    /// Completed-only nearest-rank p99.
    pub completed_p99_ns: u64,
    /// Completed-only maximum.
    pub completed_max_ns: u64,
    /// Completed samples strictly below 50ms.
    pub completed_under50: u64,
    /// `completed_under50 / admitted` report-only ratio.
    pub completed_under50_over_admitted: f64,
    /// `completed_under50 / completed` report-only ratio.
    pub completed_under50_over_completed: f64,
}

impl LatencyReport {
    /// Builds a terminal report from completed-only samples.
    pub fn from_terminal(
        accounting: LatencyAccounting,
        mut completed_ns: Vec<u64>,
    ) -> Result<Self, LatencyError> {
        accounting.validate()?;
        if accounting.admitted == 0 || accounting.completed == 0 {
            return Err(LatencyError::ZeroDenominator);
        }
        if usize::try_from(accounting.completed).ok() != Some(completed_ns.len()) {
            return Err(LatencyError::SampleCountMismatch);
        }
        completed_ns.sort_unstable();
        let completed_under50 =
            completed_ns.iter().filter(|latency| **latency < LATENCY_THRESHOLD_NS).count() as u64;
        Ok(Self {
            received: accounting.received,
            admitted: accounting.admitted,
            completed: accounting.completed,
            dropped: accounting.dropped(),
            truncated: accounting.truncated,
            in_flight: accounting.in_flight,
            completed_p50_ns: Self::nearest_rank(&completed_ns, 50)?,
            completed_p95_ns: Self::nearest_rank(&completed_ns, 95)?,
            completed_p99_ns: Self::nearest_rank(&completed_ns, 99)?,
            completed_max_ns: *completed_ns.last().ok_or(LatencyError::ZeroDenominator)?,
            completed_under50,
            completed_under50_over_admitted: completed_under50 as f64 / accounting.admitted as f64,
            completed_under50_over_completed: completed_under50 as f64
                / accounting.completed as f64,
        })
    }

    /// Returns the nearest-rank percentile `sorted[ceil(p*N)-1]`.
    pub fn nearest_rank(sorted: &[u64], percentile: usize) -> Result<u64, LatencyError> {
        if sorted.is_empty() || percentile == 0 || percentile > 100 {
            return Err(LatencyError::ZeroDenominator);
        }
        let numerator =
            percentile.checked_mul(sorted.len()).ok_or(LatencyError::InvalidAccounting)?;
        let rank = numerator.checked_add(99).ok_or(LatencyError::InvalidAccounting)? / 100;
        sorted.get(rank - 1).copied().ok_or(LatencyError::InvalidAccounting)
    }

    /// Returns FULL only for the exclusive p95 and nominal accounting gate.
    pub const fn is_full(&self) -> bool {
        self.admitted >= LATENCY_TIMED_RUNS as u64
            && self.completed == self.admitted
            && self.dropped == 0
            && self.truncated == 0
            && self.in_flight == 0
            && self.completed_p95_ns < LATENCY_THRESHOLD_NS
    }
}

/// Five adjacent stage durations and their receive-to-encode total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageLatencySample {
    /// Snapshot discovery duration.
    pub discover_ns: u64,
    /// Prepared-state construction and canonicalization duration.
    pub canonicalize_ns: u64,
    /// Processed-frame binding duration.
    pub bind_ns: u64,
    /// Two-hop discovery and selection duration.
    pub two_hop_ns: u64,
    /// Final evidence encoding duration.
    pub encode_ns: u64,
    /// Receive-to-encode duration.
    pub end_to_end_ns: u64,
}

impl StageLatencySample {
    /// Builds a sample from monotonically ordered monotonic-clock checkpoints.
    pub fn from_checkpoints(
        t0: Instant,
        discover_end: Instant,
        canonicalize_end: Instant,
        bind_end: Instant,
        two_hop_end: Instant,
        encode_end: Instant,
    ) -> Result<Self, LatencyError> {
        let discover_ns =
            discover_end.checked_duration_since(t0).ok_or(LatencyError::InvalidAccounting)?;
        let canonicalize_ns = canonicalize_end
            .checked_duration_since(discover_end)
            .ok_or(LatencyError::InvalidAccounting)?;
        let bind_ns = bind_end
            .checked_duration_since(canonicalize_end)
            .ok_or(LatencyError::InvalidAccounting)?;
        let two_hop_ns =
            two_hop_end.checked_duration_since(bind_end).ok_or(LatencyError::InvalidAccounting)?;
        let encode_ns = encode_end
            .checked_duration_since(two_hop_end)
            .ok_or(LatencyError::InvalidAccounting)?;
        let end_to_end_ns =
            encode_end.checked_duration_since(t0).ok_or(LatencyError::InvalidAccounting)?;
        let sample = Self {
            discover_ns: u64::try_from(discover_ns.as_nanos())
                .map_err(|_| LatencyError::InvalidAccounting)?,
            canonicalize_ns: u64::try_from(canonicalize_ns.as_nanos())
                .map_err(|_| LatencyError::InvalidAccounting)?,
            bind_ns: u64::try_from(bind_ns.as_nanos())
                .map_err(|_| LatencyError::InvalidAccounting)?,
            two_hop_ns: u64::try_from(two_hop_ns.as_nanos())
                .map_err(|_| LatencyError::InvalidAccounting)?,
            encode_ns: u64::try_from(encode_ns.as_nanos())
                .map_err(|_| LatencyError::InvalidAccounting)?,
            end_to_end_ns: u64::try_from(end_to_end_ns.as_nanos())
                .map_err(|_| LatencyError::InvalidAccounting)?,
        };
        sample.validate()?;
        Ok(sample)
    }

    /// Validates that the five stages add exactly to the end-to-end duration.
    pub fn validate(&self) -> Result<(), LatencyError> {
        let sum = self
            .discover_ns
            .checked_add(self.canonicalize_ns)
            .and_then(|value| value.checked_add(self.bind_ns))
            .and_then(|value| value.checked_add(self.two_hop_ns))
            .and_then(|value| value.checked_add(self.encode_ns))
            .ok_or(LatencyError::InvalidAccounting)?;
        if sum != self.end_to_end_ns {
            return Err(LatencyError::InvalidAccounting);
        }
        Ok(())
    }
}

/// Nullable nearest-rank quantiles for one named stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageQuantiles {
    /// Number of completed samples represented.
    pub sample_count: u64,
    /// Nearest-rank p50, or unavailable for an empty cohort.
    pub p50_ns: Option<u64>,
    /// Nearest-rank p95, or unavailable for an empty cohort.
    pub p95_ns: Option<u64>,
    /// Nearest-rank p99, or unavailable for an empty cohort.
    pub p99_ns: Option<u64>,
    /// Maximum, or unavailable for an empty cohort.
    pub max_ns: Option<u64>,
}

impl StageQuantiles {
    /// Computes nearest-rank quantiles while preserving explicit empty-cohort nullability.
    pub fn from_samples(mut samples: Vec<u64>) -> Result<Self, LatencyError> {
        let sample_count =
            u64::try_from(samples.len()).map_err(|_| LatencyError::InvalidAccounting)?;
        if samples.is_empty() {
            return Ok(Self {
                sample_count,
                p50_ns: None,
                p95_ns: None,
                p99_ns: None,
                max_ns: None,
            });
        }
        samples.sort_unstable();
        Ok(Self {
            sample_count,
            p50_ns: Some(LatencyReport::nearest_rank(&samples, 50)?),
            p95_ns: Some(LatencyReport::nearest_rank(&samples, 95)?),
            p99_ns: Some(LatencyReport::nearest_rank(&samples, 99)?),
            max_ns: samples.last().copied(),
        })
    }
}

/// Atomic staged wrapper around the original terminal latency recorder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StageLatencyRecorder {
    recorder: LatencyRecorder,
    completed_stages: Vec<StageLatencySample>,
}

impl StageLatencyRecorder {
    /// Records one pre-admission rejection through the original recorder.
    pub fn record_pre_admission_drop(&mut self) -> Result<(), LatencyError> {
        self.recorder.accounting.received.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder
            .accounting
            .pre_admission_dropped
            .checked_add(1)
            .ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.record_pre_admission_drop()
    }

    /// Records one admission through the original recorder.
    pub fn record_admission(&mut self) -> Result<(), LatencyError> {
        self.recorder.accounting.received.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.accounting.admitted.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.accounting.in_flight.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.record_admission()
    }

    /// Atomically records one valid staged completion and its derived terminal latency.
    pub fn record_completion(&mut self, sample: StageLatencySample) -> Result<(), LatencyError> {
        sample.validate()?;
        if usize::try_from(self.recorder.accounting.completed).ok()
            != Some(self.completed_stages.len())
        {
            return Err(LatencyError::SampleCountMismatch);
        }
        self.recorder.accounting.in_flight.checked_sub(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.accounting.completed.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.completed_ns.try_reserve(1).map_err(|_| LatencyError::InvalidAccounting)?;
        self.completed_stages.try_reserve(1).map_err(|_| LatencyError::InvalidAccounting)?;
        self.recorder.record_completion(sample.end_to_end_ns)?;
        self.completed_stages.push(sample);
        Ok(())
    }

    /// Records one admitted zero-output drop through the original recorder.
    pub fn record_post_admission_drop(&mut self) -> Result<(), LatencyError> {
        self.recorder.accounting.in_flight.checked_sub(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder
            .accounting
            .post_admission_dropped
            .checked_add(1)
            .ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.record_post_admission_drop()
    }

    /// Records one partial-result canary through the original recorder.
    pub fn record_truncation_canary(&mut self) -> Result<(), LatencyError> {
        self.recorder.accounting.in_flight.checked_sub(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.accounting.truncated.checked_add(1).ok_or(LatencyError::InvalidAccounting)?;
        self.recorder.record_truncation_canary()
    }

    /// Builds a drained staged report.
    pub fn finish(self) -> Result<StageLatencyReport, LatencyError> {
        StageLatencyReport::from_terminal(
            self.recorder.accounting,
            self.recorder.completed_ns,
            self.completed_stages,
        )
    }

    /// Returns current counters without producing a report.
    pub const fn accounting(&self) -> LatencyAccounting {
        self.recorder.accounting()
    }
}

/// Terminal accounting plus nullable quantiles for all five stages and end-to-end latency.
#[derive(Debug, Clone, PartialEq)]
pub struct StageLatencyReport {
    /// Every frame observed before admission.
    pub received: u64,
    /// Frames rejected before admission.
    pub pre_admission_dropped: u64,
    /// Frames accepted for bounded processing.
    pub admitted: u64,
    /// Admitted frames completed with a terminal decision.
    pub completed: u64,
    /// Admitted frames dropped after admission.
    pub post_admission_dropped: u64,
    /// Pre- plus post-admission drops.
    pub dropped: u64,
    /// Partial outputs, required to remain zero for FULL.
    pub truncated: u64,
    /// Terminal in-flight count, required to be zero.
    pub in_flight: u64,
    /// Discovery-stage quantiles.
    pub discover: StageQuantiles,
    /// Canonicalization-stage quantiles.
    pub canonicalize: StageQuantiles,
    /// Binding-stage quantiles.
    pub bind: StageQuantiles,
    /// Two-hop-stage quantiles.
    pub two_hop: StageQuantiles,
    /// Final-encoding-stage quantiles.
    pub encode: StageQuantiles,
    /// Receive-to-encode quantiles.
    pub end_to_end: StageQuantiles,
    /// Original terminal report when admitted and completed denominators are nonzero.
    pub latency_report: Option<LatencyReport>,
    /// Strictly-under-50ms completions divided by admitted, when admitted is nonzero.
    pub completed_under50_over_admitted: Option<f64>,
    /// Strictly-under-50ms completions divided by completed, when completed is nonzero.
    pub completed_under50_over_completed: Option<f64>,
    /// Existing FULL gate conjoined with valid, equal staged samples.
    pub full: bool,
}

impl StageLatencyReport {
    /// Builds a staged report from original terminal samples and matching staged samples.
    pub fn from_terminal(
        accounting: LatencyAccounting,
        completed_ns: Vec<u64>,
        samples: Vec<StageLatencySample>,
    ) -> Result<Self, LatencyError> {
        accounting.validate()?;
        if usize::try_from(accounting.completed).ok() != Some(samples.len())
            || completed_ns.len() != samples.len()
        {
            return Err(LatencyError::SampleCountMismatch);
        }
        for sample in &samples {
            sample.validate()?;
        }
        if completed_ns
            .iter()
            .zip(&samples)
            .any(|(terminal_ns, sample)| *terminal_ns != sample.end_to_end_ns)
        {
            return Err(LatencyError::SampleCountMismatch);
        }

        let discover = StageQuantiles::from_samples(
            samples.iter().map(|sample| sample.discover_ns).collect(),
        )?;
        let canonicalize = StageQuantiles::from_samples(
            samples.iter().map(|sample| sample.canonicalize_ns).collect(),
        )?;
        let bind =
            StageQuantiles::from_samples(samples.iter().map(|sample| sample.bind_ns).collect())?;
        let two_hop =
            StageQuantiles::from_samples(samples.iter().map(|sample| sample.two_hop_ns).collect())?;
        let encode =
            StageQuantiles::from_samples(samples.iter().map(|sample| sample.encode_ns).collect())?;
        let end_to_end = StageQuantiles::from_samples(
            samples.iter().map(|sample| sample.end_to_end_ns).collect(),
        )?;
        let stage_counts_equal = [
            discover.sample_count,
            canonicalize.sample_count,
            bind.sample_count,
            two_hop.sample_count,
            encode.sample_count,
            end_to_end.sample_count,
        ]
        .into_iter()
        .all(|count| count == accounting.completed);
        if !stage_counts_equal {
            return Err(LatencyError::SampleCountMismatch);
        }

        let latency_report = if accounting.admitted > 0 && accounting.completed > 0 {
            Some(LatencyReport::from_terminal(accounting, completed_ns)?)
        } else {
            None
        };
        let completed_under50_over_admitted = if accounting.admitted == 0 {
            None
        } else {
            Some(
                latency_report
                    .as_ref()
                    .map_or(0.0, |report| report.completed_under50_over_admitted),
            )
        };
        let completed_under50_over_completed =
            latency_report.as_ref().map(|report| report.completed_under50_over_completed);
        let full = latency_report.as_ref().is_some_and(LatencyReport::is_full)
            && stage_counts_equal
            && samples.iter().all(|sample| sample.validate().is_ok());

        Ok(Self {
            received: accounting.received,
            pre_admission_dropped: accounting.pre_admission_dropped,
            admitted: accounting.admitted,
            completed: accounting.completed,
            post_admission_dropped: accounting.post_admission_dropped,
            dropped: accounting.dropped(),
            truncated: accounting.truncated,
            in_flight: accounting.in_flight,
            discover,
            canonicalize,
            bind,
            two_hop,
            encode,
            end_to_end,
            latency_report,
            completed_under50_over_admitted,
            completed_under50_over_completed,
            full,
        })
    }

    /// Returns the staged FULL verdict.
    pub const fn is_full(&self) -> bool {
        self.full
    }
}
#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc, time::Duration};

    use alloy_primitives::{Address, U256};
    use serde::{Deserialize, Deserializer, Serialize};
    use serde_json::Value;

    use super::*;
    use crate::{
        CancellationProbe, CancellationToken, ExactProtocol, GlobalLifecycle, MeasurementEncoder,
        PairwiseEngine, PairwiseError, PreparedPoolQuote, PreparedPoolState, TaskState, WETH,
        frame::test_utils::TestFrameHarness,
    };

    const OPERATION_CONTRACT: &str = "discover=capture;canonicalize=prepared-fixture+validate;bind=successful-process-proof;two_hop=discover+proof-bound-select(internal-digest-encode);encode=one-final-evidence-encode;validate=untimed-post-encode";

    fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum EvidenceScope {
        #[serde(rename = "local-only")]
        LocalOnly,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum EvidenceClock {
        #[serde(rename = "std::time::Instant")]
        Instant,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum EvidenceT0 {
        #[serde(rename = "VictimFrame.received_at")]
        FrameReceivedAt,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum WorkloadCohort {
        #[serde(rename = "warm")]
        Warm,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum ConcurrentLoad {
        #[serde(rename = "none")]
        None,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum PhaseBValue {
        #[serde(rename = "UNKNOWN")]
        Unknown,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceWorkload {
        cohort: WorkloadCohort,
        warmup_samples: u64,
        timed_samples: u64,
        external_frame_concurrency: u64,
        analysis_threads: u64,
        concurrent_load: ConcurrentLoad,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceCounts {
        received: u64,
        pre_admission_dropped: u64,
        admitted: u64,
        completed: u64,
        post_admission_dropped: u64,
        dropped: u64,
        truncated: u64,
        in_flight: u64,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceQuantiles {
        sample_count: u64,
        #[serde(deserialize_with = "required_option")]
        p50_ns: Option<u64>,
        #[serde(deserialize_with = "required_option")]
        p95_ns: Option<u64>,
        #[serde(deserialize_with = "required_option")]
        p99_ns: Option<u64>,
        #[serde(deserialize_with = "required_option")]
        max_ns: Option<u64>,
    }

    impl From<StageQuantiles> for EvidenceQuantiles {
        fn from(value: StageQuantiles) -> Self {
            Self {
                sample_count: value.sample_count,
                p50_ns: value.p50_ns,
                p95_ns: value.p95_ns,
                p99_ns: value.p99_ns,
                max_ns: value.max_ns,
            }
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceStages {
        discover: EvidenceQuantiles,
        canonicalize: EvidenceQuantiles,
        bind: EvidenceQuantiles,
        two_hop: EvidenceQuantiles,
        encode: EvidenceQuantiles,
        end_to_end: EvidenceQuantiles,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceRatio {
        numerator: u64,
        denominator: u64,
        #[serde(deserialize_with = "required_option")]
        ratio: Option<f64>,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceUnder50 {
        admitted_all: EvidenceRatio,
        completed_only: EvidenceRatio,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidencePhaseB {
        sign: PhaseBValue,
        sequencer: PhaseBValue,
        attribution: PhaseBValue,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LatencyEvidence {
        schema: String,
        base_commit: String,
        inspected_a1_commit: String,
        scope: EvidenceScope,
        clock: EvidenceClock,
        t0: EvidenceT0,
        operation_contract: String,
        workload: EvidenceWorkload,
        counts: EvidenceCounts,
        stages_ns: EvidenceStages,
        under_50ms: EvidenceUnder50,
        full: bool,
        phase_b: EvidencePhaseB,
    }

    fn live_probe() -> CancellationProbe {
        CancellationProbe::new(
            Arc::new(CancellationToken::with_approved_deadline(Instant::now())),
            Arc::new(GlobalLifecycle::default()),
        )
    }

    fn prepared_fixture() -> ([PreparedPoolState; 2], [Address; 1]) {
        let token = Address::with_last_byte(0xaa);
        let first_pool = Address::with_last_byte(1);
        let second_pool = Address::with_last_byte(2);
        (
            [
                PreparedPoolState {
                    pool: first_pool,
                    protocol: ExactProtocol::UniswapV2,
                    token0: WETH,
                    token1: token,
                    decimals0: 18,
                    decimals1: 18,
                    fee_pips: 3_000,
                    quote: PreparedPoolQuote::ConstantProduct {
                        reserve0: U256::from(1_000_000_000_000_000_000_000_000_u128),
                        reserve1: U256::from(2_000_000_000_000_000_000_000_000_u128),
                    },
                },
                PreparedPoolState {
                    pool: second_pool,
                    protocol: ExactProtocol::UniswapV2,
                    token0: WETH,
                    token1: token,
                    decimals0: 18,
                    decimals1: 18,
                    fee_pips: 3_000,
                    quote: PreparedPoolQuote::ConstantProduct {
                        reserve0: U256::from(1_000_000_000_000_000_000_000_000_u128),
                        reserve1: U256::from(1_000_000_000_000_000_000_000_000_u128),
                    },
                },
            ],
            [first_pool],
        )
    }

    fn real_path() -> (StageLatencySample, Vec<u8>) {
        let selection_probe = live_probe();
        let (harness, discover_end) = TestFrameHarness::capture_timed();

        let (pools, dirty_pools) = prepared_fixture();
        for pool in &pools {
            pool.validate().expect("canonical prepared pool");
        }
        let canonicalize_end = Instant::now();

        let processed = harness.process_prepared();
        let bind_end = Instant::now();
        let processed = processed.expect("frame processing").expect("successful frame proof");

        let candidates =
            PairwiseEngine::discover("a2-latency", &pools, &dirty_pools, &selection_probe)
                .expect("pairwise discovery");
        let plan = PairwiseEngine::select_measurement(&processed, &candidates, &selection_probe);
        let two_hop_end = Instant::now();
        let plan = plan.expect("proof-bound selection").expect("positive plan");

        let bytes = MeasurementEncoder::encode(&plan);
        let encode_end = Instant::now();

        let bytes = bytes.expect("final evidence encoding");
        MeasurementEncoder::validate(&plan).expect("untimed post-encode validation");
        assert!(!bytes.is_empty());
        assert!(!plan.digest.0.is_zero());
        harness.assert_processed(&processed);

        let sample = StageLatencySample::from_checkpoints(
            harness.frame().received_at,
            discover_end,
            canonicalize_end,
            bind_end,
            two_hop_end,
            encode_end,
        )
        .expect("ordered real-path checkpoints");
        (sample, bytes)
    }

    fn assert_exact_keys(value: &Value, expected: &[&str]) {
        let actual = value
            .as_object()
            .expect("JSON object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected.iter().copied().collect());
    }

    fn sample_with_end_to_end(end_to_end_ns: u64) -> StageLatencySample {
        StageLatencySample {
            discover_ns: end_to_end_ns,
            canonicalize_ns: 0,
            bind_ns: 0,
            two_hop_ns: 0,
            encode_ns: 0,
            end_to_end_ns,
        }
    }

    #[test]
    fn checkpoints_accept_ordered_and_equal_boundaries_with_exact_sum() {
        let t0 = Instant::now();
        let at = |nanoseconds| {
            t0.checked_add(Duration::from_nanos(nanoseconds)).expect("representable checkpoint")
        };
        let sample = StageLatencySample::from_checkpoints(t0, at(1), at(3), at(6), at(10), at(15))
            .expect("ordered");
        assert_eq!(
            sample,
            StageLatencySample {
                discover_ns: 1,
                canonicalize_ns: 2,
                bind_ns: 3,
                two_hop_ns: 4,
                encode_ns: 5,
                end_to_end_ns: 15,
            }
        );
        assert_eq!(
            sample.discover_ns
                + sample.canonicalize_ns
                + sample.bind_ns
                + sample.two_hop_ns
                + sample.encode_ns,
            sample.end_to_end_ns
        );

        let equal = StageLatencySample::from_checkpoints(t0, t0, t0, t0, t0, t0)
            .expect("equal checkpoints");
        assert_eq!(equal, sample_with_end_to_end(0));
    }

    #[test]
    fn checkpoints_and_manual_samples_reject_ordering_sum_and_overflow_errors() {
        let t0 = Instant::now();
        let earlier = t0.checked_sub(Duration::from_nanos(1)).expect("earlier");
        assert_eq!(
            StageLatencySample::from_checkpoints(t0, earlier, t0, t0, t0, t0),
            Err(LatencyError::InvalidAccounting)
        );
        let mismatch = StageLatencySample {
            discover_ns: 1,
            canonicalize_ns: 2,
            bind_ns: 3,
            two_hop_ns: 4,
            encode_ns: 5,
            end_to_end_ns: 14,
        };
        assert_eq!(mismatch.validate(), Err(LatencyError::InvalidAccounting));

        let overflow = StageLatencySample {
            discover_ns: u64::MAX,
            canonicalize_ns: 1,
            bind_ns: 0,
            two_hop_ns: 0,
            encode_ns: 0,
            end_to_end_ns: u64::MAX,
        };
        assert_eq!(overflow.validate(), Err(LatencyError::InvalidAccounting));
    }

    #[test]
    fn stage_quantiles_use_exact_nearest_rank_and_nullable_empty_shape() {
        let quantiles = StageQuantiles::from_samples((1..=100).collect()).expect("stage quantiles");
        assert_eq!(quantiles.sample_count, 100);
        assert_eq!(quantiles.p50_ns, Some(50));
        assert_eq!(quantiles.p95_ns, Some(95));
        assert_eq!(quantiles.p99_ns, Some(99));
        assert_eq!(quantiles.max_ns, Some(100));

        let empty = StageQuantiles::from_samples(Vec::new()).expect("empty quantiles");
        assert_eq!(
            empty,
            StageQuantiles {
                sample_count: 0,
                p50_ns: None,
                p95_ns: None,
                p99_ns: None,
                max_ns: None,
            }
        );
    }

    #[test]
    fn staged_completion_is_atomic_and_derives_terminal_latency_from_sample() {
        let mut recorder = StageLatencyRecorder::default();
        recorder.record_admission().expect("admit");
        let before = recorder.accounting();
        let invalid = StageLatencySample { end_to_end_ns: 2, ..sample_with_end_to_end(1) };
        assert_eq!(recorder.record_completion(invalid), Err(LatencyError::InvalidAccounting));
        assert_eq!(recorder.accounting(), before);
        assert!(recorder.completed_stages.is_empty());
        assert!(recorder.recorder.completed_ns.is_empty());

        let valid = sample_with_end_to_end(49);
        recorder.record_completion(valid).expect("complete");
        assert_eq!(recorder.accounting().completed, 1);
        assert_eq!(recorder.completed_stages, vec![valid]);
        assert_eq!(recorder.recorder.completed_ns, vec![valid.end_to_end_ns]);
    }

    #[test]
    fn staged_completion_prevalidation_leaves_overflowing_counters_unchanged() {
        let mut recorder = StageLatencyRecorder::default();
        recorder.recorder.accounting =
            LatencyAccounting { received: u64::MAX, ..LatencyAccounting::default() };
        let before = recorder.clone();
        assert_eq!(recorder.record_pre_admission_drop(), Err(LatencyError::InvalidAccounting));
        assert_eq!(recorder, before);
    }

    #[test]
    fn staged_report_rejects_sample_mismatch_invalid_conservation_and_non_drain() {
        let completed_without_sample = LatencyAccounting {
            received: 1,
            admitted: 1,
            completed: 1,
            ..LatencyAccounting::default()
        };
        assert_eq!(
            StageLatencyReport::from_terminal(completed_without_sample, Vec::new(), Vec::new(),),
            Err(LatencyError::SampleCountMismatch)
        );

        assert_eq!(
            StageLatencyReport::from_terminal(
                completed_without_sample,
                vec![2],
                vec![sample_with_end_to_end(1)],
            ),
            Err(LatencyError::SampleCountMismatch)
        );

        let invalid_conservation = LatencyAccounting {
            received: 2,
            pre_admission_dropped: 1,
            ..LatencyAccounting::default()
        };
        assert_eq!(
            StageLatencyReport::from_terminal(invalid_conservation, Vec::new(), Vec::new(),),
            Err(LatencyError::InvalidAccounting)
        );

        let not_drained = LatencyAccounting {
            received: 1,
            admitted: 1,
            in_flight: 1,
            ..LatencyAccounting::default()
        };
        assert_eq!(
            StageLatencyReport::from_terminal(not_drained, Vec::new(), Vec::new()),
            Err(LatencyError::NotDrained)
        );
    }

    #[test]
    fn split_drops_and_truncation_remain_visible_in_zero_completed_report() {
        let mut recorder = StageLatencyRecorder::default();
        recorder.record_pre_admission_drop().expect("pre drop");
        recorder.record_admission().expect("admit post drop");
        recorder.record_post_admission_drop().expect("post drop");
        recorder.record_admission().expect("admit truncation");
        recorder.record_truncation_canary().expect("truncate");

        let report = recorder.finish().expect("report");
        assert_eq!(report.received, 3);
        assert_eq!(report.pre_admission_dropped, 1);
        assert_eq!(report.admitted, 2);
        assert_eq!(report.completed, 0);
        assert_eq!(report.post_admission_dropped, 1);
        assert_eq!(report.dropped, 2);
        assert_eq!(report.truncated, 1);
        assert_eq!(report.in_flight, 0);
        assert_eq!(report.end_to_end.sample_count, 0);
        assert_eq!(report.end_to_end.p95_ns, None);
        assert_eq!(report.completed_under50_over_admitted, Some(0.0));
        assert_eq!(report.completed_under50_over_completed, None);
        assert_eq!(report.latency_report, None);
        assert!(!report.is_full());
    }

    #[test]
    fn all_zero_completed_terminal_cohorts_have_honest_nullable_ratios() {
        let empty = StageLatencyRecorder::default().finish().expect("empty");
        assert_eq!(empty.completed_under50_over_admitted, None);
        assert_eq!(empty.completed_under50_over_completed, None);
        assert_eq!(empty.discover.sample_count, 0);
        assert_eq!(empty.discover.p50_ns, None);

        let mut all_pre_drop = StageLatencyRecorder::default();
        all_pre_drop.record_pre_admission_drop().expect("pre drop");
        let all_pre_drop = all_pre_drop.finish().expect("all pre drop");
        assert_eq!(all_pre_drop.completed_under50_over_admitted, None);
        assert_eq!(all_pre_drop.completed_under50_over_completed, None);
        assert!(!all_pre_drop.full);

        let mut all_post_drop = StageLatencyRecorder::default();
        all_post_drop.record_admission().expect("admit");
        all_post_drop.record_post_admission_drop().expect("post drop");
        let all_post_drop = all_post_drop.finish().expect("all post drop");
        assert_eq!(all_post_drop.completed_under50_over_admitted, Some(0.0));
        assert_eq!(all_post_drop.completed_under50_over_completed, None);
        assert!(!all_post_drop.full);

        let mut all_truncated = StageLatencyRecorder::default();
        all_truncated.record_admission().expect("admit");
        all_truncated.record_truncation_canary().expect("truncate");
        let all_truncated = all_truncated.finish().expect("all truncated");
        assert_eq!(all_truncated.completed_under50_over_admitted, Some(0.0));
        assert_eq!(all_truncated.completed_under50_over_completed, None);
        assert!(!all_truncated.full);
    }

    #[test]
    fn staged_report_reuses_strict_threshold_and_dual_denominators() {
        let mut recorder = StageLatencyRecorder::default();
        for latency in [10, 20] {
            recorder.record_admission().expect("admit completion");
            recorder.record_completion(sample_with_end_to_end(latency)).expect("complete");
        }
        for _ in 0..2 {
            recorder.record_admission().expect("admit drop");
            recorder.record_post_admission_drop().expect("drop");
        }

        let report = recorder.finish().expect("report");
        let terminal = report.latency_report.as_ref().expect("terminal report");
        assert_eq!(terminal.completed_under50, 2);
        assert_eq!(report.completed_under50_over_admitted, Some(0.5));
        assert_eq!(report.completed_under50_over_completed, Some(1.0));
        assert!(!report.full);
    }

    #[test]
    fn exactly_fifty_milliseconds_is_excluded_and_prevents_full() {
        let mut recorder = StageLatencyRecorder::default();
        for _ in 0..LATENCY_TIMED_RUNS {
            recorder.record_admission().expect("admit");
            recorder
                .record_completion(sample_with_end_to_end(LATENCY_THRESHOLD_NS))
                .expect("complete");
        }
        let report = recorder.finish().expect("report");
        let terminal = report.latency_report.as_ref().expect("terminal report");
        assert_eq!(terminal.completed_under50, 0);
        assert_eq!(terminal.completed_p95_ns, LATENCY_THRESHOLD_NS);
        assert!(!terminal.is_full());
        assert!(!report.is_full());
    }

    #[test]
    fn staged_full_is_the_existing_full_gate_conjoined_with_stage_validity() {
        let mut recorder = StageLatencyRecorder::default();
        for latency in 1..=LATENCY_TIMED_RUNS as u64 {
            recorder.record_admission().expect("admit");
            recorder.record_completion(sample_with_end_to_end(latency)).expect("complete");
        }
        let report = recorder.finish().expect("report");
        assert!(report.latency_report.as_ref().expect("terminal").is_full());
        assert!(report.is_full());
        for quantiles in [
            report.discover,
            report.canonicalize,
            report.bind,
            report.two_hop,
            report.encode,
            report.end_to_end,
        ] {
            assert_eq!(quantiles.sample_count, LATENCY_TIMED_RUNS as u64);
        }
    }

    #[test]
    fn existing_latency_apis_keep_nearest_rank_threshold_and_denominators() {
        let mut recorder = LatencyRecorder::default();
        for latency in 1..=LATENCY_TIMED_RUNS as u64 {
            recorder.record_admission().expect("admit");
            recorder.record_completion(latency).expect("complete");
        }
        let report = recorder.finish().expect("report");
        assert_eq!(report.completed_p50_ns, 50);
        assert_eq!(report.completed_p95_ns, 95);
        assert_eq!(report.completed_p99_ns, 99);
        assert!(report.is_full());

        let accounting = LatencyAccounting {
            received: 4,
            admitted: 4,
            completed: 2,
            post_admission_dropped: 2,
            ..LatencyAccounting::default()
        };
        let report = LatencyReport::from_terminal(accounting, vec![10, 20]).expect("report");
        assert_eq!(report.completed_under50_over_admitted, 0.5);
        assert_eq!(report.completed_under50_over_completed, 1.0);
        assert!(!report.is_full());

        assert_eq!(
            LatencyReport::from_terminal(LatencyAccounting::default(), Vec::new()),
            Err(LatencyError::ZeroDenominator)
        );
    }

    #[test]
    fn proof_bound_selector_cancellation_drops_without_a_plan() {
        let processed = crate::frame::test_utils::processed_frame();
        let (pools, dirty_pools) = prepared_fixture();
        for pool in &pools {
            pool.validate().expect("canonical prepared pool");
        }
        let candidates = PairwiseEngine::discover("cancelled", &pools, &dirty_pools, &live_probe())
            .expect("pairwise discovery");

        let token = Arc::new(CancellationToken::with_approved_deadline(Instant::now()));
        assert!(token.request_cancel());
        let cancelled =
            CancellationProbe::new(Arc::clone(&token), Arc::new(GlobalLifecycle::default()));
        let mut plan_count = 0;
        let result = PairwiseEngine::select_measurement(&processed, &candidates, &cancelled);
        if matches!(&result, Ok(Some(_))) {
            plan_count += 1;
        }

        assert_eq!(result, Err(PairwiseError::Cancelled));
        assert_eq!(plan_count, 0);
        assert_eq!(token.state(), TaskState::DroppedAcked);
    }

    #[test]
    #[ignore = "run once as the offline release latency gate"]
    fn release_fixture_uses_ten_warmups_one_hundred_samples_and_drains() {
        assert_eq!(LATENCY_WARMUP_RUNS, 10);
        assert_eq!(LATENCY_TIMED_RUNS, 100);
        for _ in 0..LATENCY_WARMUP_RUNS {
            let _ = real_path();
        }

        let mut recorder = StageLatencyRecorder::default();
        for _ in 0..LATENCY_TIMED_RUNS {
            recorder.record_admission().expect("timed admission");
            let (sample, bytes) = real_path();
            assert!(!bytes.is_empty());
            recorder.record_completion(sample).expect("atomic timed completion");
        }

        let report = recorder.finish().expect("drained staged report");
        assert_eq!(report.received, 100);
        assert_eq!(report.pre_admission_dropped, 0);
        assert_eq!(report.admitted, 100);
        assert_eq!(report.completed, 100);
        assert_eq!(report.post_admission_dropped, 0);
        assert_eq!(report.dropped, 0);
        assert_eq!(report.truncated, 0);
        assert_eq!(report.in_flight, 0);
        for quantiles in [
            report.discover,
            report.canonicalize,
            report.bind,
            report.two_hop,
            report.encode,
            report.end_to_end,
        ] {
            assert_eq!(quantiles.sample_count, 100);
            assert!(quantiles.p50_ns.is_some());
            assert!(quantiles.p95_ns.is_some());
            assert!(quantiles.p99_ns.is_some());
            assert!(quantiles.max_ns.is_some());
        }
        let terminal = report.latency_report.as_ref().expect("terminal latency report");
        assert!(terminal.completed_p95_ns < LATENCY_THRESHOLD_NS);
        assert!(report.is_full());
        assert_eq!(terminal.completed_under50, 100);
        assert_eq!(report.completed_under50_over_admitted, Some(1.0));
        assert_eq!(report.completed_under50_over_completed, Some(1.0));

        let evidence = LatencyEvidence {
            schema: "mev-trader-a2-latency-v1".to_owned(),
            base_commit: "b36dfa6f".to_owned(),
            inspected_a1_commit: "f5ab569d".to_owned(),
            scope: EvidenceScope::LocalOnly,
            clock: EvidenceClock::Instant,
            t0: EvidenceT0::FrameReceivedAt,
            operation_contract: OPERATION_CONTRACT.to_owned(),
            workload: EvidenceWorkload {
                cohort: WorkloadCohort::Warm,
                warmup_samples: LATENCY_WARMUP_RUNS as u64,
                timed_samples: LATENCY_TIMED_RUNS as u64,
                external_frame_concurrency: 1,
                analysis_threads: 0,
                concurrent_load: ConcurrentLoad::None,
            },
            counts: EvidenceCounts {
                received: report.received,
                pre_admission_dropped: report.pre_admission_dropped,
                admitted: report.admitted,
                completed: report.completed,
                post_admission_dropped: report.post_admission_dropped,
                dropped: report.dropped,
                truncated: report.truncated,
                in_flight: report.in_flight,
            },
            stages_ns: EvidenceStages {
                discover: report.discover.into(),
                canonicalize: report.canonicalize.into(),
                bind: report.bind.into(),
                two_hop: report.two_hop.into(),
                encode: report.encode.into(),
                end_to_end: report.end_to_end.into(),
            },
            under_50ms: EvidenceUnder50 {
                admitted_all: EvidenceRatio {
                    numerator: terminal.completed_under50,
                    denominator: report.admitted,
                    ratio: report.completed_under50_over_admitted,
                },
                completed_only: EvidenceRatio {
                    numerator: terminal.completed_under50,
                    denominator: report.completed,
                    ratio: report.completed_under50_over_completed,
                },
            },
            full: report.full,
            phase_b: EvidencePhaseB {
                sign: PhaseBValue::Unknown,
                sequencer: PhaseBValue::Unknown,
                attribution: PhaseBValue::Unknown,
            },
        };

        let value = serde_json::to_value(&evidence).expect("evidence value");
        assert_exact_keys(
            &value,
            &[
                "schema",
                "base_commit",
                "inspected_a1_commit",
                "scope",
                "clock",
                "t0",
                "operation_contract",
                "workload",
                "counts",
                "stages_ns",
                "under_50ms",
                "full",
                "phase_b",
            ],
        );
        assert_exact_keys(
            &value["workload"],
            &[
                "cohort",
                "warmup_samples",
                "timed_samples",
                "external_frame_concurrency",
                "analysis_threads",
                "concurrent_load",
            ],
        );
        assert_exact_keys(
            &value["counts"],
            &[
                "received",
                "pre_admission_dropped",
                "admitted",
                "completed",
                "post_admission_dropped",
                "dropped",
                "truncated",
                "in_flight",
            ],
        );
        assert_exact_keys(
            &value["stages_ns"],
            &["discover", "canonicalize", "bind", "two_hop", "encode", "end_to_end"],
        );
        for stage in ["discover", "canonicalize", "bind", "two_hop", "encode", "end_to_end"] {
            assert_exact_keys(
                &value["stages_ns"][stage],
                &["sample_count", "p50_ns", "p95_ns", "p99_ns", "max_ns"],
            );
            for quantile in ["p50_ns", "p95_ns", "p99_ns", "max_ns"] {
                assert!(!value["stages_ns"][stage][quantile].is_null());
            }
        }
        assert_exact_keys(&value["under_50ms"], &["admitted_all", "completed_only"]);
        for ratio in ["admitted_all", "completed_only"] {
            assert_exact_keys(&value["under_50ms"][ratio], &["numerator", "denominator", "ratio"]);
            assert!(!value["under_50ms"][ratio]["ratio"].is_null());
        }
        assert_exact_keys(&value["phase_b"], &["sign", "sequencer", "attribution"]);

        let nullable = serde_json::to_value(EvidenceQuantiles {
            sample_count: 0,
            p50_ns: None,
            p95_ns: None,
            p99_ns: None,
            max_ns: None,
        })
        .expect("nullable quantiles");
        assert_exact_keys(&nullable, &["sample_count", "p50_ns", "p95_ns", "p99_ns", "max_ns"]);
        for quantile in ["p50_ns", "p95_ns", "p99_ns", "max_ns"] {
            assert!(nullable[quantile].is_null());
        }

        let mut missing_nullable = nullable;
        missing_nullable.as_object_mut().expect("quantiles object").remove("p50_ns");
        assert!(serde_json::from_value::<EvidenceQuantiles>(missing_nullable).is_err());

        let nullable_ratio =
            serde_json::to_value(EvidenceRatio { numerator: 0, denominator: 0, ratio: None })
                .expect("nullable ratio");
        assert_exact_keys(&nullable_ratio, &["numerator", "denominator", "ratio"]);
        assert!(nullable_ratio["ratio"].is_null());

        let mut missing_ratio = nullable_ratio;
        missing_ratio.as_object_mut().expect("ratio object").remove("ratio");
        assert!(serde_json::from_value::<EvidenceRatio>(missing_ratio).is_err());

        let mut with_extra = value.clone();
        with_extra
            .as_object_mut()
            .expect("top-level object")
            .insert("unexpected".to_owned(), Value::Null);
        assert!(serde_json::from_value::<LatencyEvidence>(with_extra).is_err());
        assert_eq!(
            serde_json::from_value::<LatencyEvidence>(value).expect("typed round trip"),
            evidence
        );

        let compact = serde_json::to_string(&evidence).expect("compact evidence JSON");
        assert!(!compact.contains('\n'));
        for forbidden in [
            "\"command\"",
            "\"exit\"",
            "\"host\"",
            "\"path\"",
            "\"status\"",
            "self-hash",
            "self_hash",
            "rawTx",
            "credential",
            "private key",
            "private_key",
            "\"url\"",
            "\"endpoint\"",
        ] {
            assert!(!compact.contains(forbidden), "forbidden evidence content: {forbidden}");
        }
        println!("{compact}");
    }
}
