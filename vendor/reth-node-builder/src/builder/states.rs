//! Node builder states and helper traits.
//!
//! Keeps track of the current state of the node builder.
//!
//! The node builder process is essentially a state machine that transitions through various states
//! before the node can be launched.

use std::{fmt::Debug, future::Future};

use base_execution_evm::BaseEvmConfig;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_exex::ExExContext;
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_core::node_config::NodeConfig;
use reth_provider::providers::{BlockchainProvider, RocksDBProvider};
use reth_tasks::TaskExecutor;

use crate::{
    FullNode,
    components::ComponentBuilder,
    hooks::NodeHooks,
    launch::LaunchNode,
    rpc::{RethRpcAddOns, RethRpcServerHandles, RpcContext},
};

/// Container for the node's types and the components and other internals that can be used by
/// addons of the node.
#[derive(Debug, Clone)]
pub struct NodeAdapter<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> {
    /// The node transaction pool.
    pub transaction_pool: reth_node_api::BaseNodePool<BlockchainProvider<DB>>,
    /// The Base EVM configuration.
    pub evm_config: BaseEvmConfig,
    /// The Base consensus validator.
    pub consensus: std::sync::Arc<base_execution_consensus::BaseBeaconConsensus>,
    /// The network handle.
    pub network: reth_network::NetworkHandle,
    /// The payload service handle.
    pub payload_builder_handle: base_execution_payload_builder::PayloadBuilderHandle,
    /// The task executor for the node.
    pub task_executor: TaskExecutor,
    /// The provider of the node.
    pub provider: BlockchainProvider<DB>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> FullNodeComponents
    for NodeAdapter<DB>
{
    type DB = DB;
    type Provider = BlockchainProvider<DB>;
    fn pool(&self) -> &reth_node_api::BaseNodePool<Self::Provider> {
        &self.transaction_pool
    }

    fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    fn consensus(&self) -> &std::sync::Arc<base_execution_consensus::BaseBeaconConsensus> {
        &self.consensus
    }

    fn network(&self) -> &reth_network::NetworkHandle {
        &self.network
    }

    fn payload_builder_handle(&self) -> &base_execution_payload_builder::PayloadBuilderHandle {
        &self.payload_builder_handle
    }

    fn provider(&self) -> &Self::Provider {
        &self.provider
    }

    fn task_executor(&self) -> &TaskExecutor {
        &self.task_executor
    }
}

/// A fully type configured node builder.
///
/// Supports adding additional addons to the node.
pub struct NodeBuilderWithComponents<DB, AO>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    AO: NodeAddOns<NodeAdapter<DB>>,
{
    /// All settings for how the node should be configured.
    pub config: NodeConfig,
    /// Adapter for the underlying node types and database
    pub database: DB,
    /// An optional [`RocksDBProvider`] to use instead of creating one during launch.
    pub rocksdb_provider: Option<RocksDBProvider>,
    /// container for type specific components
    pub components_builder: ComponentBuilder<DB>,
    /// Additional node extensions.
    pub add_ons: AO,
    /// Hooks invoked as the node starts.
    pub hooks: NodeHooks<NodeAdapter<DB>, AO>,
    /// Execution extensions installed on this node.
    pub exexs: Vec<(String, Box<dyn crate::exex::BoxedLaunchExEx<NodeAdapter<DB>>>)>,
}

impl<DB> NodeBuilderWithComponents<DB, ()>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    /// Advances the state of the node builder to the next state where all customizable
    /// [`NodeAddOns`] types are configured.
    pub fn with_add_ons<AO>(self, add_ons: AO) -> NodeBuilderWithComponents<DB, AO>
    where
        AO: NodeAddOns<NodeAdapter<DB>>,
    {
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

impl<DB, AO> NodeBuilderWithComponents<DB, AO>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    AO: NodeAddOns<NodeAdapter<DB>>,
{
    /// Sets the hook that is run once the node's components are initialized.
    pub fn on_component_initialized<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(NodeAdapter<DB>) -> eyre::Result<()> + Send + 'static,
    {
        self.hooks.set_on_component_initialized(hook);
        self
    }

    /// Sets the hook that is run once the node has started.
    pub fn on_node_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(FullNode<NodeAdapter<DB>, AO>) -> eyre::Result<()> + Send + 'static,
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
        F: FnOnce(ExExContext<NodeAdapter<DB>>) -> R + Send + 'static,
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

    /// Modifies the addons with the given closure.
    ///
    /// This method provides access to methods on the addons type that don't have
    /// direct builder methods. It's useful for advanced configuration scenarios
    /// where you need to call addon-specific methods.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use tower::layer::util::Identity;
    ///
    /// let builder = NodeBuilder::new(config)
    ///
    ///     .with_components(BaseNode::components())
    ///     .with_add_ons(BaseAddOns::default())
    ///     .map_add_ons(|addons| addons.with_rpc_middleware(Identity::default()));
    /// ```
    ///
    /// # See also
    ///
    /// - [`NodeAddOns`] trait for available addon types
    /// - [`crate::NodeBuilderWithComponents::extend_rpc_modules`] for RPC module configuration
    pub fn map_add_ons<F>(mut self, f: F) -> Self
    where
        F: FnOnce(AO) -> AO,
    {
        self.add_ons = f(self.add_ons);
        self
    }
}

impl<DB, AO> NodeBuilderWithComponents<DB, AO>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    AO: RethRpcAddOns<NodeAdapter<DB>>,
{
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
                RpcContext<'_, NodeAdapter<DB>, AO::EthApi>,
                RethRpcServerHandles,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.map_add_ons(|mut add_ons| {
            add_ons.hooks_mut().set_on_rpc_started(hook);
            add_ons
        })
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub fn extend_rpc_modules<F>(self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, NodeAdapter<DB>, AO::EthApi>) -> eyre::Result<()> + Send + 'static,
    {
        self.map_add_ons(|mut add_ons| {
            add_ons.hooks_mut().set_extend_rpc_modules(hook);
            add_ons
        })
    }
}
