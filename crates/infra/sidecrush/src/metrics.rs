use cadence::{Counted, StatsdClient};
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

    /// Increment status_delayed counter (2s heartbeat)
    pub fn increment_status_delayed(&self) {
        let _ = self.client.count("status_delayed", 1);
    }

    /// Increment status_unhealthy counter (2s heartbeat)
    pub fn increment_status_unhealthy(&self) {
        let _ = self.client.count("status_unhealthy", 1);
    }

    /// Increment status_error counter (2s heartbeat)
    pub fn increment_status_error(&self) {
        let _ = self.client.count("status_error", 1);
    }
}
