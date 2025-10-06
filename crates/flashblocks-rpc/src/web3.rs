use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};

use reth::core::primitives::constants::RETH_CLIENT_VERSION;
use tracing::debug;

use crate::metrics::Metrics;

/// The node-reth client version: `base/v{major}.{minor}.{patch}`
/// E.g. `base/v0.1.10`
pub const NODE_RETH_CLIENT_VERSION: &str = concat!("base/v", env!("CARGO_PKG_VERSION"));

#[cfg_attr(not(test), rpc(server, namespace = "web3"))]
#[cfg_attr(test, rpc(server, client, namespace = "web3"))]
pub trait Web3ApiOverride {
    #[method(name = "clientVersion")]
    async fn client_version(&self) -> RpcResult<String>;
}

#[derive(Debug)]
pub struct Web3ApiExt {
    metrics: Metrics,
}

impl Default for Web3ApiExt {
    fn default() -> Self {
        Self::new()
    }
}

impl Web3ApiExt {
    pub fn new() -> Self {
        Self {
            metrics: Metrics::default(),
        }
    }
}

#[async_trait]
impl Web3ApiOverrideServer for Web3ApiExt {
    async fn client_version(&self) -> RpcResult<String> {
        debug!(message = "web3::client_version");
        self.metrics.web3_client_version.increment(1);

        // reth/v{major}.{minor}.{patch}/base/v{major}.{minor}.{patch}
        let client_version = format!("{}/{}", RETH_CLIENT_VERSION, NODE_RETH_CLIENT_VERSION);
        Ok(client_version)
    }
}
