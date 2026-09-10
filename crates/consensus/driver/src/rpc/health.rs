use async_trait::async_trait;
use base_common_client_rollup::HealthzApiServer;
use base_common_types_rpc::HealthzResponse;
use jsonrpsee::core::RpcResult;

/// The healthz rpc server.
#[derive(Debug, Clone)]
pub struct HealthzRpc {}

#[async_trait]
impl HealthzApiServer for HealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        Ok(HealthzResponse { version: env!("CARGO_PKG_VERSION").to_string() })
    }
}
