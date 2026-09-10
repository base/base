//! Wall-clock scheduled ticker that records its own drift on every fire.
//!
//! Wraps [`tokio::time::Interval`] together with the wall-clock target time
//! requested at the last reset. When the interval fires, the elapsed time
//! between target and actual fire is recorded to
//! [`Metrics::sequencer_ticker_drift_seconds`]. Early fires record `0`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::{Instant, Interval};

use crate::Metrics;

/// A [`tokio::time::Interval`] that remembers its wall-clock target so the
/// drift between intended and actual fire time can be observed transparently
/// every tick.
#[derive(Debug)]
pub struct ScheduledTicker {
    interval: Interval,
    target: Option<SystemTime>,
    immediate_l1_origin_retries: u8,
}

impl ScheduledTicker {
    /// Delay between next-origin build attempts after the immediate retry budget is exhausted.
    pub const L1_ORIGIN_RETRY_DELAY: Duration = Duration::from_millis(200);

    /// Number of immediate next-origin retries allowed before applying the retry delay.
    pub const MAX_IMMEDIATE_L1_ORIGIN_RETRIES: u8 = 5;

    /// Creates a new ticker with the given period.
    ///
    /// The first fire occurs immediately, mirroring [`tokio::time::interval`]
    /// semantics. No target is set, so the first tick records no drift.
    pub fn new(period: Duration) -> Self {
        Self {
            interval: tokio::time::interval(period),
            target: None,
            immediate_l1_origin_retries: 0,
        }
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

    /// Reschedules the next tick for a Unix timestamp in seconds.
    ///
    /// If the timestamp has already passed, the ticker fires immediately.
    pub fn reset_at_unix_timestamp(&mut self, timestamp: u64) {
        self.reset_at(UNIX_EPOCH + Duration::from_secs(timestamp));
    }

    /// Reschedules the next tick to fire `lead_time` before a Unix timestamp.
    ///
    /// If the resulting target has already passed, the ticker fires immediately.
    pub fn reset_before_unix_timestamp(&mut self, timestamp: u64, lead_time: Duration) {
        self.reset_at(UNIX_EPOCH + Duration::from_secs(timestamp) - lead_time);
    }

    /// Reschedules the next tick to fire immediately, with `now` as the
    /// drift target (so the recorded drift is approximately zero plus any
    /// scheduler latency).
    pub fn reset_immediately(&mut self) {
        self.reset_at(SystemTime::now());
    }

    /// Reschedules the ticker based on the outcome of a build attempt.
    ///
    /// If `target` is `Some`, schedules for that wall-clock time. If `None` (the build was
    /// deferred or discarded), fires immediately so the next attempt refreshes state instead of
    /// retrying a stale target.
    pub fn schedule_after_build(&mut self, target: Option<SystemTime>) {
        match target {
            Some(target) => self.reset_at(target),
            None => self.reset_immediately(),
        }
    }

    /// Resets the immediate retry budget after a build succeeds.
    ///
    /// Reset-triggered and other deferrals intentionally preserve the budget so persistent L1
    /// provider lag or repeated engine resets cannot hot-loop the sequencer.
    pub const fn reset_l1_origin_retry_budget(&mut self) {
        self.immediate_l1_origin_retries = 0;
    }

    /// Schedules a retry after a non-fatal build deferral or while awaiting a prefetched origin.
    ///
    /// A short burst handles races where the background fetch is about to complete. Once the
    /// budget is exhausted, subsequent retries are spaced by [`Self::L1_ORIGIN_RETRY_DELAY`] so
    /// the actor remains responsive without hot-looping the build path.
    pub fn schedule_l1_origin_retry(&mut self) {
        if self.immediate_l1_origin_retries < Self::MAX_IMMEDIATE_L1_ORIGIN_RETRIES {
            self.immediate_l1_origin_retries += 1;
            self.reset_immediately();
        } else {
            self.reset_at(SystemTime::now() + Self::L1_ORIGIN_RETRY_DELAY);
        }
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
            let drift = SystemTime::now().duration_since(target).unwrap_or(Duration::ZERO);
            Metrics::sequencer_ticker_drift_seconds().record(drift);
        }
        instant
    }

    /// Returns the wall-clock target for the next tick.
    #[cfg(test)]
    pub const fn target(&self) -> Option<SystemTime> {
        self.target
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::ScheduledTicker;

    #[tokio::test]
    async fn schedule_after_build_uses_target_when_built() {
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        let target = UNIX_EPOCH + Duration::from_millis(2_000_000_000);

        ticker.schedule_after_build(Some(target));

        assert_eq!(ticker.target(), Some(target));
    }

    #[tokio::test]
    async fn schedule_after_build_fires_immediately_when_no_payload_was_built() {
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

        ticker.schedule_after_build(None);

        assert!(ticker.target().is_some_and(|target| target <= SystemTime::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn reset_at_past_target_fires_immediately() {
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));
        ticker.tick().await;

        // A seal target already in the past (e.g. the previous block overran its slot) must
        // make the ticker immediately runnable rather than waiting a full period.
        ticker.reset_at(SystemTime::now() - Duration::from_secs(1));

        tokio::time::timeout(Duration::from_millis(1), ticker.tick())
            .await
            .expect("past target must fire immediately");
    }

    #[tokio::test]
    async fn l1_origin_retry_backs_off_after_immediate_retry_budget() {
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

        for _ in 0..ScheduledTicker::MAX_IMMEDIATE_L1_ORIGIN_RETRIES {
            ticker.schedule_l1_origin_retry();
            assert!(ticker.target().is_some_and(|target| target <= SystemTime::now()));
        }

        let before_backoff = SystemTime::now();
        ticker.schedule_l1_origin_retry();

        assert!(ticker.target().is_some_and(|target| {
            target >= before_backoff + ScheduledTicker::L1_ORIGIN_RETRY_DELAY
        }));
    }

    #[tokio::test]
    async fn l1_origin_retry_budget_can_be_reset() {
        let mut ticker = ScheduledTicker::new(Duration::from_secs(2));

        for _ in 0..=ScheduledTicker::MAX_IMMEDIATE_L1_ORIGIN_RETRIES {
            ticker.schedule_l1_origin_retry();
        }
        ticker.reset_l1_origin_retry_budget();
        ticker.schedule_l1_origin_retry();

        assert!(ticker.target().is_some_and(|target| target <= SystemTime::now()));
    }
}
