//! Contains the [`Backend`] trait, which defines
//! the interface for backends in Roxy.

use async_trait::async_trait;
use alloy_json_rpc::{RequestPacket, ResponsePacket};

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
