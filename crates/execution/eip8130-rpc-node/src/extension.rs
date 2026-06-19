//! `BaseNodeExtension` that registers the standalone EIP-8130
//! `eth_getTransactionCount` override when flashblocks is not.

use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

/// Whether [`Eip8130RpcExtension`] should register the standalone EIP-8130
/// `eth_getTransactionCount` override, or defer to another extension
/// (flashblocks) that owns the same RPC method.
///
/// Both this extension and the flashblocks extension call
/// `ctx.modules.replace_configured` on `eth_getTransactionCount`, and
/// `replace_configured` is overwrite, so the node-assembly site must
/// designate exactly one owner via this mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eip8130RpcMode {
    /// Register the standalone override. Use on nodes without flashblocks.
    Register,
    /// Skip registration. Use on nodes where flashblocks is registering
    /// `eth_getTransactionCount` and thereby owning the EIP-8130 RPC surface.
    Defer,
}

/// Wires the standalone EIP-8130 `eth_getTransactionCount` override
/// into the node, gated by [`Eip8130RpcMode`].
#[derive(Debug)]
pub struct Eip8130RpcExtension {
    mode: Eip8130RpcMode,
}

impl Eip8130RpcExtension {
    /// Creates a new extension. Pass [`Eip8130RpcMode::Register`] on
    /// nodes without flashblocks; pass [`Eip8130RpcMode::Defer`] when
    /// flashblocks is already registering the override.
    pub const fn new(mode: Eip8130RpcMode) -> Self {
        Self { mode }
    }
}

impl BaseNodeExtension for Eip8130RpcExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        match self.mode {
            Eip8130RpcMode::Defer => {
                info!(message = "EIP-8130 RPC override deferred to flashblocks");
                hooks
            }
            Eip8130RpcMode::Register => hooks.add_rpc_module(|ctx| {
                info!(message = "Starting standalone EIP-8130 RPC override");
                let api_ext = Eip8130EthApiExt::new(ctx.registry.eth_api().clone());
                ctx.modules.replace_configured(api_ext.into_rpc())?;
                Ok(())
            }),
        }
    }
}

impl FromExtensionConfig for Eip8130RpcExtension {
    type Config = Eip8130RpcMode;

    fn from_config(mode: Eip8130RpcMode) -> Self {
        Self::new(mode)
    }
}
