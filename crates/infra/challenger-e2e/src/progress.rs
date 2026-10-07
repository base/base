//! Where a run is, and what it has covered so far.
//!
//! The outcome logs say what each phase proved. These say where the run *is*:
//! a phase's start and finish, with its position in the scenario's plan, how
//! long it took, and at the end one summary of every planned phase. A failed
//! or hung run then reads as "Path 3 started, step 3/5, still waiting" rather
//! than as a gap in the stream, and a phase a branch never reached shows up as
//! skipped instead of as an absence nobody notices.

use std::{fmt::Write as _, time::Instant};

use tracing::{error, info};

use crate::{
    challenger_e2e::{Phase, Verdict},
    config::Scenario,
};

/// Every phase a scenario can assert, in the order it runs them.
///
/// Branches can leave a phase out: `all` reaches Path 2 skip only when Path 1
/// lands as a ZK challenge, and Path 3 only when Path 4 drops the TEE proof
/// first. Those are recorded as skipped, so the summary always lists the whole
/// plan.
pub(crate) const fn plan(scenario: Scenario) -> &'static [Phase] {
    match scenario {
        Scenario::All => &[
            Phase::Setup,
            Phase::QuietWindow,
            Phase::Path1,
            Phase::Path2Skip,
            Phase::Path4,
            Phase::Path3,
            Phase::Bystanders,
        ],
        Scenario::Path1Path2 => &[
            Phase::Setup,
            Phase::QuietWindow,
            Phase::Path1,
            Phase::Path2Skip,
            Phase::Path2Dispute,
            Phase::Bystanders,
        ],
        Scenario::Path3 => &[Phase::Setup, Phase::QuietWindow, Phase::Path3, Phase::Bystanders],
    }
}

/// Logs one numbered step inside a phase, e.g. `Path 3 step 2/5: ...`.
pub(crate) fn step(phase: Phase, step: u32, steps: u32, what: &str) {
    info!(phase = %phase, step, steps, what, "{} step {step}/{steps}: {what}", phase.label());
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    phase: Phase,
    verdict: Verdict,
    elapsed_ms: Option<u128>,
}

/// Tracks the phases of one run against its scenario's plan.
#[derive(Debug)]
pub(crate) struct Progress {
    plan: &'static [Phase],
    current: Option<(Phase, Instant)>,
    outcomes: Vec<Outcome>,
    started: Instant,
}

impl Progress {
    /// Starts tracking a run of `scenario`.
    pub(crate) fn new(scenario: Scenario) -> Self {
        Self { plan: plan(scenario), current: None, outcomes: Vec::new(), started: Instant::now() }
    }

    /// 1-based position of `phase` in the plan.
    fn position(&self, phase: Phase) -> usize {
        self.plan.iter().position(|p| *p == phase).map_or(0, |i| i + 1)
    }

    /// Marks `phase` as running.
    pub(crate) fn start(&mut self, phase: Phase) {
        let step = self.position(phase);
        let steps = self.plan.len();
        info!(phase = %phase, step, steps, "{} started ({step}/{steps})", phase.label());
        self.current = Some((phase, Instant::now()));
    }

    /// Marks the running phase as passed.
    pub(crate) fn pass(&mut self) {
        let Some((phase, since)) = self.current.take() else { return };
        let elapsed_ms = since.elapsed().as_millis();
        info!(
            phase = %phase,
            verdict = %Verdict::Pass,
            elapsed_ms,
            "{} finished",
            phase.label()
        );
        self.outcomes.push(Outcome { phase, verdict: Verdict::Pass, elapsed_ms: Some(elapsed_ms) });
    }

    /// Records `phase` as asserted inside another phase rather than on its own.
    pub(crate) fn passed_within(&mut self, phase: Phase, within: Phase) {
        info!(
            phase = %phase,
            verdict = %Verdict::Pass,
            within = %within,
            "{} finished (asserted during {})",
            phase.label(),
            within.label()
        );
        self.outcomes.push(Outcome { phase, verdict: Verdict::Pass, elapsed_ms: None });
    }

    /// Records a planned phase this run's branch did not reach.
    pub(crate) fn skip(&mut self, phase: Phase, reason: &str) {
        info!(
            phase = %phase,
            verdict = %Verdict::Skip,
            reason,
            "{} skipped: {reason}",
            phase.label()
        );
        self.outcomes.push(Outcome { phase, verdict: Verdict::Skip, elapsed_ms: None });
    }

    /// Marks the running phase as the one that failed.
    ///
    /// An error raised between two phases, with none running, is charged to
    /// the next planned phase that has no outcome yet, so a failed run never
    /// reads `failed=0`.
    pub(crate) fn fail(&mut self, cause: &eyre::Report) {
        let (phase, elapsed_ms) = match self.current.take() {
            Some((phase, since)) => (phase, Some(since.elapsed().as_millis())),
            None => {
                let next = self
                    .plan
                    .iter()
                    .find(|phase| !self.outcomes.iter().any(|o| o.phase == **phase));
                let Some(&phase) = next else {
                    error!(error = %cause, "run failed after every planned phase");
                    return;
                };
                (phase, None)
            }
        };
        error!(
            phase = %phase,
            verdict = %Verdict::Fail,
            elapsed_ms,
            error = %cause,
            "{} failed",
            phase.label()
        );
        self.outcomes.push(Outcome { phase, verdict: Verdict::Fail, elapsed_ms });
    }

    /// One line per planned phase, e.g. `path3=pass(22.4s)`; a phase the run
    /// never got to reads `not-run`.
    fn render(&self) -> String {
        let mut out = String::new();
        for phase in self.plan {
            if !out.is_empty() {
                out.push(' ');
            }
            match self.outcomes.iter().find(|o| o.phase == *phase) {
                Some(Outcome { verdict, elapsed_ms: Some(ms), .. }) => {
                    let _ = write!(out, "{phase}={verdict}({:.1}s)", *ms as f64 / 1000.0);
                }
                Some(Outcome { verdict, elapsed_ms: None, .. }) => {
                    let _ = write!(out, "{phase}={verdict}");
                }
                None => {
                    let _ = write!(out, "{phase}=not-run");
                }
            }
        }
        out
    }

    /// Logs the summary of every planned phase.
    pub(crate) fn summary(&self) {
        let count = |verdict| self.outcomes.iter().filter(|o| o.verdict == verdict).count();
        info!(
            results = %self.render(),
            passed = count(Verdict::Pass),
            skipped = count(Verdict::Skip),
            failed = count(Verdict::Fail),
            planned = self.plan.len(),
            total_ms = self.started.elapsed().as_millis(),
            "run summary"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_plan_starts_with_setup_and_ends_with_bystanders() {
        for scenario in [Scenario::All, Scenario::Path1Path2, Scenario::Path3] {
            let plan = plan(scenario);
            assert_eq!(plan.first(), Some(&Phase::Setup), "{scenario:?}");
            assert_eq!(plan.last(), Some(&Phase::Bystanders), "{scenario:?}");
        }
    }

    #[test]
    fn summary_lists_the_whole_plan() {
        let mut progress = Progress::new(Scenario::Path3);
        progress.start(Phase::Setup);
        progress.pass();
        progress.start(Phase::QuietWindow);
        progress.fail(&eyre::eyre!("boom"));

        let rendered = progress.render();
        assert!(rendered.starts_with("setup=pass("), "{rendered}");
        assert!(rendered.contains(" quiet-window=fail("), "{rendered}");
        assert!(rendered.ends_with(" path3=not-run bystanders=not-run"), "{rendered}");
    }

    #[test]
    fn skipped_and_nested_phases_carry_no_duration() {
        let mut progress = Progress::new(Scenario::All);
        progress.skip(Phase::Path2Skip, "Path 1 landed as a TEE nullify");
        progress.passed_within(Phase::Path3, Phase::Path4);

        let rendered = progress.render();
        assert!(rendered.contains("path2-skip=skip "), "{rendered}");
        assert!(rendered.contains("path3=pass "), "{rendered}");
    }

    #[test]
    fn an_error_between_phases_fails_the_next_planned_phase() {
        let mut progress = Progress::new(Scenario::Path1Path2);
        for phase in [Phase::Setup, Phase::QuietWindow, Phase::Path1] {
            progress.start(phase);
            progress.pass();
        }
        progress.skip(Phase::Path2Skip, "Path 1 landed as a TEE nullify");

        progress.fail(&eyre::eyre!("Path 2 dispute requires Path 1 to land as a ZK challenge"));

        let rendered = progress.render();
        assert!(rendered.contains("path2-dispute=fail "), "{rendered}");
        assert!(rendered.ends_with("bystanders=not-run"), "{rendered}");
    }

    #[test]
    fn positions_follow_the_plan() {
        let progress = Progress::new(Scenario::All);
        assert_eq!(progress.position(Phase::Setup), 1);
        assert_eq!(progress.position(Phase::Path4), 5);
        assert_eq!(progress.position(Phase::Path2Dispute), 0);
    }
}
