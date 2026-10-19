use std::sync::Arc;

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
}
