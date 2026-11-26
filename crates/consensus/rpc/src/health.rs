use async_trait::async_trait;
use base_consensus_rpc_types::{HealthzApiServer, HealthzResponse};
use jsonrpsee::core::RpcResult;

/// The healthz rpc server.
#[derive(Debug, Clone)]
pub struct HealthzRpc;

#[async_trait]
impl HealthzApiServer for HealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        Ok(HealthzResponse { version: env!("CARGO_PKG_VERSION").to_string() })
    }
}
