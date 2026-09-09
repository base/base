use async_trait::async_trait;
use base_common_types_rpc::HealthzResponse;
use jsonrpsee::core::RpcResult;

use crate::jsonrpsee::HealthzApiServer;

/// The healthz rpc server.
#[derive(Debug, Clone)]
pub struct HealthzRpc {}

#[async_trait]
impl HealthzApiServer for HealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        Ok(HealthzResponse { version: env!("CARGO_PKG_VERSION").to_string() })
    }
}
