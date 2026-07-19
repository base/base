//! Deterministic latency measurement and reporting.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_is_exact_and_p95_equality_is_not_full() {
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
            received: 100,
            admitted: 100,
            completed: 100,
            ..LatencyAccounting::default()
        };
        let equality = LatencyReport::from_terminal(
            accounting,
            vec![LATENCY_THRESHOLD_NS; LATENCY_TIMED_RUNS],
        )
        .expect("equality report");
        assert!(!equality.is_full());
        assert_eq!(equality.completed_under50, 0);
    }

    #[test]
    fn ratios_keep_admitted_and_completed_denominators_distinct() {
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
    }

    #[test]
    fn denominator_zero_and_nonterminal_accounting_are_invalid() {
        assert_eq!(
            LatencyReport::from_terminal(LatencyAccounting::default(), Vec::new()),
            Err(LatencyError::ZeroDenominator)
        );
        let not_drained = LatencyAccounting {
            received: 1,
            admitted: 1,
            in_flight: 1,
            ..LatencyAccounting::default()
        };
        assert_eq!(not_drained.validate(), Err(LatencyError::NotDrained));
    }
}
