//! Registers the EIP-8130 transaction-count RPC override.

use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};

/// Registers the EIP-8130 transaction-count RPC override.
#[derive(Debug, Default)]
pub struct Eip8130RpcExtension;

impl BaseNodeExtension for Eip8130RpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(|ctx| {
            let api_ext = Eip8130EthApiExt::new(ctx.registry.eth_api().clone());
            ctx.modules.replace_configured(api_ext.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for Eip8130RpcExtension {
    type Config = ();

    fn from_config((): Self::Config) -> Self {
        Self
    }
}
