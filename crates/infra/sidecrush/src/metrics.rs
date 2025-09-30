use once_cell::sync::Lazy;
use reth_metrics::{metrics::Counter, metrics::Gauge, Metrics};

#[derive(Metrics, Clone)]
#[metrics(scope = "base.blocks")]
pub struct HealthcheckMetrics {
    #[metric(describe = "Heartbeat healthy (2s)")]
    pub status_healthy: Counter,
    #[metric(describe = "Heartbeat unhealthy (2s)")]
    pub status_unhealthy: Counter,
    #[metric(describe = "Heartbeat error (2s)")]
    pub status_error: Counter,

    #[metric(describe = "Error polls (non-new-instance)")]
    pub error: Counter,
    #[metric(describe = "Healthy polls")]
    pub healthy: Counter,
    #[metric(describe = "Delayed polls")]
    pub delayed: Counter,
    #[metric(describe = "Unhealthy polls")]
    pub unhealthy: Counter,

    #[metric(describe = "Head age in milliseconds")]
    pub head_age_ms: Gauge,
}

pub static METRICS: Lazy<HealthcheckMetrics> = Lazy::new(HealthcheckMetrics::default);
pub use METRICS as metrics;
