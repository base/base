//! Wall-clock scheduled ticker that records its own drift on every fire.
//!
//! Wraps [`tokio::time::Interval`] together with the wall-clock target time
//! requested at the last reset. When the interval fires, the elapsed time
//! between target and actual fire is recorded to
//! [`Metrics::sequencer_ticker_drift_seconds`]. Early fires record `0`.

use std::time::{Duration, SystemTime};

use tokio::time::{Instant, Interval};

use crate::Metrics;

/// A [`tokio::time::Interval`] that remembers its wall-clock target so the
/// drift between intended and actual fire time can be observed transparently
/// every tick.
#[derive(Debug)]
pub struct ScheduledTicker {
    interval: Interval,
    target: Option<SystemTime>,
}

impl ScheduledTicker {
    /// Creates a new ticker with the given period.
    ///
    /// The first fire occurs immediately, mirroring [`tokio::time::interval`]
    /// semantics. No target is set, so the first tick records no drift.
    pub fn new(period: Duration) -> Self {
        Self { interval: tokio::time::interval(period), target: None }
    }

    /// Reschedules the next tick for the given wall-clock target.
    ///
    /// If `target` is in the past the ticker fires immediately. The next
    /// [`Self::tick`] will record the drift between `target` and the actual
    /// fire time.
    pub fn reset_at(&mut self, target: SystemTime) {
        self.target = Some(target);
        match target.duration_since(SystemTime::now()) {
            Ok(duration) => self.interval.reset_after(duration),
            Err(_) => self.interval.reset_immediately(),
        }
    }

    /// Reschedules the next tick to fire immediately, with `now` as the
    /// drift target (so the recorded drift is approximately zero plus any
    /// scheduler latency).
    pub fn reset_immediately(&mut self) {
        self.reset_at(SystemTime::now());
    }

    /// Awaits the next tick.
    ///
    /// On fire, records [`Metrics::sequencer_ticker_drift_seconds`] using the
    /// target from the last [`Self::reset_at`] / [`Self::reset_immediately`]
    /// call. Early fires (target in the future) are clamped to
    /// [`Duration::ZERO`]. Ticks with no prior target (e.g. the very first
    /// tick after construction) record nothing.
    pub async fn tick(&mut self) -> Instant {
        let instant = self.interval.tick().await;
        if let Some(target) = self.target.take() {
            let drift =
                SystemTime::now().duration_since(target).unwrap_or(Duration::ZERO);
            Metrics::sequencer_ticker_drift_seconds().record(drift);
        }
        instant
    }
}
