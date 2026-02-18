//! Contains the [`Backend`] and [`HealthTracker`] traits, which define
//! the interfaces for backends and health tracking in Roxy.

use std::time::Duration;

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use async_trait::async_trait;

use crate::{HealthStatus, RoxyError};

/// Core backend trait for RPC forwarding.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Backend identifier.
    fn name(&self) -> &str;

    /// RPC endpoint URL.
    fn rpc_url(&self) -> &str;

    /// Forward RPC request packet (single or batch).
    async fn forward(&self, request: RequestPacket) -> Result<ResponsePacket, RoxyError>;

    /// Current health status.
    fn health_status(&self) -> HealthStatus;

    /// Latency EMA for load balancing.
    fn latency_ema(&self) -> Duration;

    /// Whether backend should receive requests.
    fn is_healthy(&self) -> bool {
        matches!(self.health_status(), HealthStatus::Healthy | HealthStatus::Degraded { .. })
    }
}

/// Health tracking with EMA.
pub trait HealthTracker: Send + Sync {
    /// Record a request result.
    fn record(&mut self, duration: Duration, success: bool);

    /// Get latency EMA.
    fn latency_ema(&self) -> Duration;

    /// Get error rate (0.0 to 1.0).
    fn error_rate(&self) -> f64;

    /// Get current health status.
    fn status(&self) -> HealthStatus;
}
