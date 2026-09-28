//! Wall-clock timing for system-test stack startup phases.

use std::time::Instant;

use tracing::info;

/// Logs how long each startup phase of a system-test stack takes.
///
/// Each [`finish`](Self::finish) call emits one structured event with the phase's own duration
/// and the time elapsed since the timer started, so slow phases can be ranked across a test run.
#[derive(Debug)]
pub struct SetupPhaseTimer {
    started: Instant,
    phase_started: Instant,
}

impl SetupPhaseTimer {
    /// Starts timing the first phase.
    pub fn start() -> Self {
        let now = Instant::now();
        Self { started: now, phase_started: now }
    }

    /// Records the end of `phase` and starts timing the next one.
    pub fn finish(&mut self, phase: &'static str) {
        let now = Instant::now();
        info!(
            phase,
            elapsed_ms = now.duration_since(self.phase_started).as_millis() as u64,
            total_ms = now.duration_since(self.started).as_millis() as u64,
            "system test setup phase finished"
        );
        self.phase_started = now;
    }
}
