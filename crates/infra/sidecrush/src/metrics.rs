use cadence::{Counted, Gauged, StatsdClient};
use std::sync::Arc;

/// Metrics client wrapper for block building health checks
/// Emits metrics every 2 seconds via status heartbeat (independent of poll frequency)
#[derive(Clone, Debug)]
pub struct HealthcheckMetrics {
    client: Arc<StatsdClient>,
}

impl HealthcheckMetrics {
    pub fn new(client: StatsdClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    /// Increment status_healthy counter (2s heartbeat)
    pub fn increment_status_healthy(&self) {
        let _ = self.client.count("status_healthy", 1);
    }

    /// Increment status_unhealthy counter (2s heartbeat)
    pub fn increment_status_unhealthy(&self) {
        let _ = self.client.count("status_unhealthy", 1);
    }

    /// Increment status_error counter (2s heartbeat)
    pub fn increment_status_error(&self) {
        let _ = self.client.count("status_error", 1);
    }

    /// Set head_age_ms gauge
    pub fn set_head_age_ms(&self, value: u64) {
        let _ = self.client.gauge("head_age_ms", value);
    }
}
