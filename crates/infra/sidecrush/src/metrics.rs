use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use cadence::{CountedExt, StatsdClient};

use crate::HealthState;

/// Metrics client wrapper for block building health checks.
///
/// Emits metrics every 2 seconds via status heartbeat (independent of poll frequency).
#[derive(Clone, Debug)]
pub struct HealthcheckMetrics {
    client: Arc<StatsdClient>,
}

impl HealthcheckMetrics {
    /// Wrap a pre-configured [`StatsdClient`] (with prefix, tags, and sink already set) in an
    /// `Arc` so the emitter task can share it with the health-check poller.
    pub fn new(client: StatsdClient) -> Self {
        Self { client: Arc::new(client) }
    }

    /// Increment the counter named after the state's lowercase name, such as `healthy`.
    pub fn increment_status(&self, state: HealthState) {
        let name = match state {
            HealthState::Healthy => "healthy",
            HealthState::Delayed => "delayed",
            HealthState::Unhealthy => "unhealthy",
            HealthState::Error => "error",
        };
        let _ = self.client.incr(name);
    }

    /// Spawn a background task that publishes the [`HealthState`] stored in `status` every
    /// `period_ms` milliseconds, independent of how often the state itself is refreshed.
    /// Unknown status codes are reported as [`HealthState::Error`].
    pub fn spawn_status_emitter(
        &self,
        status: Arc<AtomicU8>,
        period_ms: u64,
    ) -> tokio::task::JoinHandle<()> {
        let metrics = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
            loop {
                ticker.tick().await;
                let code = status.load(Ordering::Relaxed);
                metrics.increment_status(HealthState::try_from(code).unwrap_or(HealthState::Error));
            }
        })
    }
}
