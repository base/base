use base_common_rpc_types::{SimBundleOverrides, SimBundleRequest, SimBundleResponse};
use jsonrpsee::proc_macros::rpc;

/// Mev rpc interface.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "mev"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "mev"))]
pub trait MevSimApi {
    /// Similar to `mev_sendBundle` but instead of submitting a bundle to the relay, it returns
    /// a simulation result. Only fully matched bundles can be simulated.
    #[method(name = "simBundle")]
    async fn sim_bundle(
        &self,
        bundle: SimBundleRequest,
        sim_overrides: SimBundleOverrides,
    ) -> jsonrpsee::core::RpcResult<SimBundleResponse>;
}
