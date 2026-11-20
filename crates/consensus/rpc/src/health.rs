#[cfg(feature = "server")]
use async_trait::async_trait;
#[cfg(feature = "server")]
use jsonrpsee::core::RpcResult;

#[cfg(feature = "server")]
use crate::jsonrpsee::HealthzApiServer;

/// A healthcheck response for the RPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HealthzResponse {
    /// The application version.
    pub version: String,
}

#[cfg(feature = "server")]
/// The healthz rpc server.
#[derive(Debug, Clone)]
pub struct HealthzRpc;

#[cfg(feature = "server")]
#[async_trait]
impl HealthzApiServer for HealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        Ok(HealthzResponse { version: env!("CARGO_PKG_VERSION").to_string() })
    }
}
