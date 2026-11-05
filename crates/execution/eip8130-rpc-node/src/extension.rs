//! `BaseNodeExtension` that registers the standalone EIP-8130 `eth_getTransactionCount`,
//! `eth_estimateGas` and `eth_call` overrides.

use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

/// Wires the standalone EIP-8130 `eth_getTransactionCount`, `eth_estimateGas`
/// and `eth_call` overrides into the node.
///
/// The overrides use reth's `TransportRpcModules::replace_configured`, which
/// overwrites all three methods on configured transports.
#[derive(Debug)]
pub struct Eip8130RpcExtension;

impl BaseNodeExtension for Eip8130RpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(|ctx| {
            info!(message = "Starting standalone EIP-8130 RPC override");
            let api_ext = Eip8130EthApiExt::new(ctx.registry.eth_api().clone());
            ctx.modules.replace_configured(api_ext.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for Eip8130RpcExtension {
    type Config = ();

    fn from_config((): ()) -> Self {
        Self
    }
}
