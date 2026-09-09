//! Node builder states and helper traits.
//!
//! Keeps track of the current state of the node builder.
//!
//! The node builder process is essentially a state machine that transitions through various states
//! before the node can be launched.

use std::future::Future;

use base_node_context::BaseNodeContext;
use reth_exex::ExExContext;
use reth_node_core::node_config::NodeConfig;
use reth_provider::providers::RocksDBProvider;

use crate::{
    FullNode,
    hooks::NodeHooks,
    launch::LaunchNode,
    launch_components::ComponentBuilder,
    rpc::{RethRpcServerHandles, RpcContext},
};

/// A fully type configured node builder.
///
/// Supports adding additional addons to the node.
pub struct NodeBuilderWithComponents {
    /// All settings for how the node should be configured.
    pub config: NodeConfig,
    /// Adapter for the underlying node types and database
    pub database: reth_db::DatabaseEnv,
    /// An optional [`RocksDBProvider`] to use instead of creating one during launch.
    pub rocksdb_provider: Option<RocksDBProvider>,
    /// container for type specific components
    pub components_builder: ComponentBuilder,
    /// Additional node extensions.
    pub add_ons: crate::BaseAddOns,
    /// Hooks invoked as the node starts.
    pub hooks: NodeHooks,
    /// Execution extensions installed on this node.
    pub exexs: Vec<(String, Box<dyn crate::exex::BoxedLaunchExEx>)>,
}

impl NodeBuilderWithComponents {
    /// Advances the state of the node builder to the next state where all customizable
    /// Base RPC services are configured.
    pub fn with_add_ons(self, add_ons: crate::BaseAddOns) -> NodeBuilderWithComponents {
        let Self { config, database, rocksdb_provider, components_builder, .. } = self;

        NodeBuilderWithComponents {
            config,
            database,
            rocksdb_provider,
            components_builder,
            add_ons,
            hooks: NodeHooks::default(),
            exexs: Vec::new(),
        }
    }
}

impl NodeBuilderWithComponents {
    /// Sets the hook that is run once the node's components are initialized.
    pub fn on_component_initialized<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(BaseNodeContext) -> eyre::Result<()> + Send + 'static,
    {
        self.hooks.set_on_component_initialized(hook);
        self
    }

    /// Sets the hook that is run once the node has started.
    pub fn on_node_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(FullNode) -> eyre::Result<()> + Send + 'static,
    {
        self.hooks.set_on_node_started(hook);
        self
    }

    /// Installs an `ExEx` (Execution Extension) in the node.
    ///
    /// # Note
    ///
    /// The `ExEx` ID must be unique.
    pub fn install_exex<F, R, E>(mut self, exex_id: impl Into<String>, exex: F) -> Self
    where
        F: FnOnce(ExExContext) -> R + Send + 'static,
        R: Future<Output = eyre::Result<E>> + Send,
        E: Future<Output = eyre::Result<()>> + Send,
    {
        self.exexs.push((exex_id.into(), Box::new(exex)));
        self
    }

    /// Launches the node with the given closure.
    pub fn launch_with_fn<L, R>(self, launcher: L) -> R
    where
        L: FnOnce(Self) -> R,
    {
        launcher(self)
    }

    /// Check that the builder can be launched
    ///
    /// This is useful when writing tests to ensure that the builder is configured correctly.
    pub const fn check_launch(self) -> Self {
        self
    }

    /// Updates Base RPC service configuration with the given closure.
    pub fn map_add_ons<F>(mut self, f: F) -> Self
    where
        F: FnOnce(crate::BaseAddOns) -> crate::BaseAddOns,
    {
        self.add_ons = f(self.add_ons);
        self
    }
}

impl NodeBuilderWithComponents {
    /// Launches the node with the given launcher.
    pub fn launch_with<L>(self, launcher: L) -> L::Future
    where
        L: LaunchNode<Self>,
    {
        launcher.launch_node(self)
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, base_execution_rpc::BaseEthApi<BaseNodeContext>>,
                RethRpcServerHandles,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.map_add_ons(|mut add_ons| {
            add_ons.rpc_add_ons.hooks.set_on_rpc_started(hook);
            add_ons
        })
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub fn extend_rpc_modules<F>(self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, base_execution_rpc::BaseEthApi<BaseNodeContext>>,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.map_add_ons(|mut add_ons| {
            add_ons.rpc_add_ons.hooks.set_extend_rpc_modules(hook);
            add_ons
        })
    }
}
