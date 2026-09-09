//! Additional `eth_` RPC API for bundles.
//!
//! See also <https://docs.flashbots.net/flashbots-auction/advanced/rpc-endpoint>

use base_common_types_rpc::{EthCallBundle, EthCallBundleResponse};
use jsonrpsee::proc_macros::rpc;

/// The `eth_callBundle` simulation API.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait EthCallBundleApi {
    /// `eth_callBundle` can be used to simulate a bundle against a specific block number,
    /// including simulating a bundle at the top of the next block.
    #[method(name = "callBundle")]
    async fn call_bundle(
        &self,
        request: EthCallBundle,
    ) -> jsonrpsee::core::RpcResult<EthCallBundleResponse>;
}
