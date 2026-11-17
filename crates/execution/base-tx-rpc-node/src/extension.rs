//! `BaseNodeExtension` that registers the Base transaction `eth_getTransactionCount`,
//! `eth_estimateGas` and `eth_call` overrides on the node.

use base_execution_base_tx_rpc::{TxBaseEthApiExt, TxBaseEthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

/// Wires the Base transaction `eth_getTransactionCount`, `eth_estimateGas` and `eth_call`
/// overrides into the node.
///
/// The overrides use reth's `TransportRpcModules::replace_configured`, which
/// overwrites all three methods, so no other extension may register them.
#[derive(Debug, Default)]
pub struct TxBaseRpcExtension;

impl BaseNodeExtension for TxBaseRpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(|ctx| {
            info!(message = "Starting Base transaction RPC override");
            let api_ext = TxBaseEthApiExt::new(ctx.registry.eth_api().clone());
            ctx.modules.replace_configured(api_ext.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for TxBaseRpcExtension {
    type Config = ();

    fn from_config((): ()) -> Self {
        Self
    }
}
