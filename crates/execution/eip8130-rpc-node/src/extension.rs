//! `BaseNodeExtension` that registers the standalone EIP-8130
//! `eth_getTransactionCount` override when flashblocks is not.

use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

/// Wires the standalone EIP-8130 `eth_getTransactionCount` override
/// into the node, gated on whether flashblocks is registering the same
/// RPC method.
#[derive(Debug)]
pub struct Eip8130RpcExtension {
    enabled: bool,
}

impl Eip8130RpcExtension {
    /// Creates a new extension. `enabled` should be `true` only on nodes
    /// where flashblocks is NOT registering its `eth_getTransactionCount`
    /// override.
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl BaseNodeExtension for Eip8130RpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        if !self.enabled {
            info!(message = "EIP-8130 RPC override skipped (flashblocks is registering it)");
            return hooks;
        }

        hooks.add_rpc_module(|ctx| {
            info!(message = "Starting standalone EIP-8130 RPC override");
            let api_ext = Eip8130EthApiExt::new(ctx.registry.eth_api().clone());
            ctx.modules.replace_configured(api_ext.into_rpc())?;
            Ok(())
        })
    }
}

impl FromExtensionConfig for Eip8130RpcExtension {
    type Config = bool;

    fn from_config(enabled: bool) -> Self {
        Self::new(enabled)
    }
}
