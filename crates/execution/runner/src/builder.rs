//! Hook accumulator for the node builder.
//!
//! [`NodeHooks`] collects RPC, node-started, and `ExEx` hooks that extensions install. These hooks
//! are applied to a configured reth builder via [`NodeHooks::apply_to`] just before launch.

use std::fmt;

use eyre::Result;
use futures::future::BoxFuture;
use reth_exex::ExExContext;
use reth_node_builder::{
    NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, WithLaunchContext,
    node::FullNode,
    rpc::{RethRpcAddOns, RpcContext},
};

use crate::types::{BaseComponentsBuilder, BaseNodeTypes, ConcreteBaseAddOns};

/// Alias for the default Base components type.
type BaseComponents = <BaseComponentsBuilder as NodeComponentsBuilder<BaseNodeTypes>>::Components;

/// Convenience alias for the Base node adapter type used by the reth builder.
///
/// Because `Components` depends only on pool, network, executor, and consensus builders (not the
/// payload service builder), this type is identical regardless of which payload service is used.
pub type BaseNodeAdapter = NodeAdapter<BaseNodeTypes, BaseComponents>;

/// Convenience alias for the OP Eth API type exposed by the reth RPC add-ons.
type BaseEthApi = <ConcreteBaseAddOns as RethRpcAddOns<BaseNodeAdapter>>::EthApi;

/// Convenience alias for the full Base node handle produced after launch.
type BaseFullNode = FullNode<BaseNodeAdapter, ConcreteBaseAddOns>;

/// Alias for the RPC context used by Base extensions.
pub type BaseRpcContext<'a> = RpcContext<'a, BaseNodeAdapter, BaseEthApi>;

/// Hook type for extending RPC modules.
type RpcModuleHook = Box<dyn FnOnce(&mut BaseRpcContext<'_>) -> Result<()> + Send + 'static>;

/// Hook type for extending add-ons.
type AddOnsHook = Box<dyn FnOnce(ConcreteBaseAddOns) -> ConcreteBaseAddOns>;

/// Hook type for node-started callbacks.
type NodeStartedHook = Box<dyn FnOnce(BaseFullNode) -> Result<()> + Send + 'static>;

/// Type-erased `ExEx` factory.
type BoxExExFactory = Box<
    dyn FnOnce(
            ExExContext<BaseNodeAdapter>,
        ) -> BoxFuture<'static, eyre::Result<BoxFuture<'static, eyre::Result<()>>>>
        + Send
        + 'static,
>;

/// A type alias for any configured builder whose components match the canonical Base types.
///
/// This is generic over the `NodeComponentsBuilder` (`CB`) so that both the default payload and
/// the flashblocks payload service can be used interchangeably.
pub type RethNodeBuilder<CB> =
    WithLaunchContext<NodeBuilderWithComponents<BaseNodeTypes, CB, ConcreteBaseAddOns>>;

/// Pure hook accumulator for the Base node builder.
///
/// Extensions call [`add_rpc_module`](Self::add_rpc_module),
/// [`add_node_started_hook`](Self::add_node_started_hook), and
/// [`install_exex`](Self::install_exex) to register hooks. The runner then calls
/// [`apply_to`](Self::apply_to) to drain all hooks onto the concrete configured builder.
///
/// After applying hooks, call [`.launch()`](RethNodeBuilder::launch) on the configured builder.
pub struct NodeHooks {
    rpc_hooks: Vec<RpcModuleHook>,
    node_started_hooks: Vec<NodeStartedHook>,
    add_ons_hooks: Vec<AddOnsHook>,
    exex_hooks: Vec<(String, BoxExExFactory)>,
}

impl NodeHooks {
    /// Create a new, empty `NodeHooks`.
    pub fn new() -> Self {
        Self {
            rpc_hooks: Vec::new(),
            node_started_hooks: Vec::new(),
            exex_hooks: Vec::new(),
            add_ons_hooks: Vec::new(),
        }
    }

    /// Applies all accumulated hooks to the given configured builder.
    ///
    /// This is generic over `CB` so that it works with any payload service whose component
    /// builder produces the same concrete `Components` type as the default OP builder.
    pub fn apply_to<CB>(self, mut builder: RethNodeBuilder<CB>) -> RethNodeBuilder<CB>
    where
        CB: NodeComponentsBuilder<BaseNodeTypes, Components = BaseComponents>,
    {
        let Self { rpc_hooks, node_started_hooks, exex_hooks, add_ons_hooks } = self;

        // Install ExEx hooks
        for (id, factory) in exex_hooks {
            builder =
                builder.install_exex(id, move |ctx: ExExContext<BaseNodeAdapter>| factory(ctx));
        }

        for hook in add_ons_hooks {
            builder = builder.map_add_ons(hook);
        }

        // Install RPC hooks
        if !rpc_hooks.is_empty() {
            builder = builder.extend_rpc_modules(move |mut ctx: BaseRpcContext<'_>| {
                for hook in rpc_hooks {
                    hook(&mut ctx)?;
                }
                Ok(())
            });
        }

        // Install node-started hooks
        if !node_started_hooks.is_empty() {
            builder = builder.on_node_started(move |full_node: BaseFullNode| {
                for hook in node_started_hooks {
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
        self.rpc_hooks.push(Box::new(hook));
        self
    }

    /// Adds an add-ons hook that will run when the add-ons are configured.
    pub fn add_add_ons_hook<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(ConcreteBaseAddOns) -> ConcreteBaseAddOns + Send + 'static,
    {
        self.add_ons_hooks.push(Box::new(hook));
        self
    }

    /// Adds a node-started hook that will run after the node has started.
    pub fn add_node_started_hook<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(BaseFullNode) -> Result<()> + Send + 'static,
    {
        self.node_started_hooks.push(Box::new(hook));
        self
    }

    /// Installs an `ExEx` extension with the given name and closure.
    pub fn install_exex<F, R, E>(mut self, exex_id: impl Into<String>, exex: F) -> Self
    where
        F: FnOnce(ExExContext<BaseNodeAdapter>) -> R + Send + 'static,
        R: Future<Output = eyre::Result<E>> + Send,
        E: Future<Output = eyre::Result<()>> + Send + 'static,
    {
        let factory: BoxExExFactory = Box::new(move |ctx| {
            Box::pin(async move {
                let inner = exex(ctx).await?;
                Ok(Box::pin(inner) as BoxFuture<'static, eyre::Result<()>>)
            })
        });
        self.exex_hooks.push((exex_id.into(), factory));
        self
    }
}

impl Default for NodeHooks {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for NodeHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHooks").finish_non_exhaustive()
    }
}
