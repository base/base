//! `BaseNodeExtension` that registers the standalone EIP-8130 `eth_getTransactionCount`,
//! `eth_estimateGas` and `eth_call` overrides on nodes without flashblocks.

use base_execution_eip8130_rpc::{Eip8130EthApiExt, Eip8130EthApiOverrideServer};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use tracing::info;

/// Whether [`Eip8130RpcExtension`] should register the standalone EIP-8130
/// `eth_getTransactionCount`, `eth_estimateGas` and `eth_call` overrides, or
/// defer to flashblocks, which owns the same three RPC methods.
///
/// Both extensions use reth's `TransportRpcModules::replace_configured`, which
/// overwrites all three methods. The node-assembly site must designate exactly
/// one owner, this extension or flashblocks, via this mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eip8130RpcMode {
    /// Register the standalone overrides. Use on nodes without flashblocks.
    Register,
    /// Skip registration. Use on nodes where flashblocks registers
    /// `eth_getTransactionCount`, `eth_estimateGas` and `eth_call`.
    Defer,
}

/// Wires the standalone EIP-8130 `eth_getTransactionCount`, `eth_estimateGas`
/// and `eth_call` overrides into the node, gated by [`Eip8130RpcMode`].
#[derive(Debug)]
pub struct Eip8130RpcExtension {
    mode: Eip8130RpcMode,
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
        Self { mode }
    }
}
