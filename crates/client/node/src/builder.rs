//! Wrapper around the OP node builder that accumulates hooks instead of replacing them.

use std::fmt;

use eyre::Result;
use reth_node_builder::{
    NodeAdapter, NodeComponentsBuilder,
    node::FullNode,
    rpc::{RethRpcAddOns, RpcContext},
};

use crate::{
    OpBuilder,
    types::{OpAddOns, OpComponentsBuilder, OpNodeTypes},
};

/// Convenience alias for the OP node adapter type used by the reth builder.
pub(crate) type OpNodeAdapter = NodeAdapter<
    OpNodeTypes,
    <OpComponentsBuilder as NodeComponentsBuilder<OpNodeTypes>>::Components,
>;

/// Convenience alias for the OP Eth API type exposed by the reth RPC add-ons.
type OpEthApi = <OpAddOns as RethRpcAddOns<OpNodeAdapter>>::EthApi;

/// Convenience alias for the full OP node handle produced after launch.
type OpFullNode = FullNode<OpNodeAdapter, OpAddOns>;

/// Alias for the RPC context used by Base extensions.
pub type BaseRpcContext<'a> = RpcContext<'a, OpNodeAdapter, OpEthApi>;

/// Hook type for extending RPC modules.
type RpcModuleHook = Box<dyn FnMut(&mut BaseRpcContext<'_>) -> Result<()> + Send + 'static>;

/// Hook type for node-started callbacks.
type NodeStartedHook = Box<dyn FnMut(OpFullNode) -> Result<()> + Send + 'static>;

/// A thin wrapper over [`OpBuilder`] that accumulates RPC and node-start hooks.
pub struct BaseBuilder {
    builder: OpBuilder,
    rpc_hooks: Vec<RpcModuleHook>,
    node_started_hooks: Vec<NodeStartedHook>,
}

impl BaseBuilder {
    /// Create a new BaseBuilder wrapping the provided OP builder.
    pub const fn new(builder: OpBuilder) -> Self {
        Self { builder, rpc_hooks: Vec::new(), node_started_hooks: Vec::new() }
    }

    /// Consumes the wrapper and returns the inner builder after installing the accumulated hooks.
    pub fn build(self) -> OpBuilder {
        let Self { mut builder, mut rpc_hooks, node_started_hooks } = self;

        if !rpc_hooks.is_empty() {
            builder = builder.extend_rpc_modules(move |mut ctx: BaseRpcContext<'_>| {
                for hook in rpc_hooks.iter_mut() {
                    hook(&mut ctx)?;
                }

                Ok(())
            });
        }

        if !node_started_hooks.is_empty() {
            builder = builder.on_node_started(move |full_node: OpFullNode| {
                let mut hooks = node_started_hooks;
                for hook in hooks.iter_mut() {
                    hook(full_node.clone())?;
                }
                Ok(())
            });
        }

        builder
    }

    /// Adds an RPC hook that will run when RPC modules are configured.
    pub fn add_rpc_module<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(&mut BaseRpcContext<'_>) -> Result<()> + Send + 'static,
    {
        let mut hook = Some(hook);
        self.rpc_hooks.push(Box::new(move |ctx| {
            if let Some(hook) = hook.take() {
                hook(ctx)?;
            }
            Ok(())
        }));
        self
    }

    /// Adds a node-started hook that will run after the node has started.
    pub fn add_node_started_hook<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(OpFullNode) -> Result<()> + Send + 'static,
    {
        let mut hook = Some(hook);
        self.node_started_hooks.push(Box::new(move |node| {
            if let Some(hook) = hook.take() {
                hook(node)?;
            }
            Ok(())
        }));
        self
    }

    /// Launches the node after applying accumulated hooks, delegating to the provided closure.
    pub fn launch_with_fn<L, R>(self, launcher: L) -> R
    where
        L: FnOnce(OpBuilder) -> R,
    {
        launcher(self.build())
    }
}

impl fmt::Debug for BaseBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseBuilder").finish_non_exhaustive()
    }
}
