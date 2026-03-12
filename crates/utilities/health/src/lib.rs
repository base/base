#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use async_trait::async_trait;
use jsonrpsee::core::RpcResult;

/// A healthcheck response for the RPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HealthzResponse {
    /// The application version.
    pub version: String,
}

/// The healthz RPC API.
#[jsonrpsee::proc_macros::rpc(server)]
pub trait HealthzApi {
    /// Returns the health status of the server.
    #[method(name = "healthz")]
    async fn healthz(&self) -> RpcResult<HealthzResponse>;
}

/// The healthz RPC server implementation.
///
/// Returns a [`HealthzResponse`] containing the version from the calling
/// crate's `CARGO_PKG_VERSION` at compile time.
#[derive(Debug, Clone)]
pub struct HealthzRpc {
    /// The version string to report.
    pub version: &'static str,
}

impl HealthzRpc {
    /// Create a new [`HealthzRpc`] with the given version.
    pub const fn new(version: &'static str) -> Self {
        Self { version }
    }
}

#[async_trait]
impl HealthzApiServer for HealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        Ok(HealthzResponse { version: self.version.to_string() })
    }
}
