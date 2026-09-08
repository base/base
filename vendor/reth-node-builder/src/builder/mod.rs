//! Customizable node builder.

#![expect(clippy::type_complexity)]
#![allow(missing_debug_implementations)]

use std::sync::Arc;

use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use base_common_consensus::BaseTxEnvelope;
use base_execution_chainspec::BaseChainSpec;
use futures::Future;
use reth_db_api::{database::Database, database_metrics::DatabaseMetrics};
use reth_exex::ExExContext;
use reth_network::{
    NetworkBuilder, NetworkConfig, NetworkConfigBuilder, NetworkHandle, NetworkManager,
    transactions::{
        TransactionPropagationPolicy, TransactionsManagerConfig,
        config::{AnnouncementFilteringPolicy, StrictEthAnnouncementFilter},
    },
};
use reth_node_api::NodeAddOns;
use reth_node_core::{
    cli::config::{PayloadBuilderConfig, RethTransactionPoolConfig},
    dirs::{ChainPath, DataDirPath},
    node_config::NodeConfig,
    primitives::Head,
};
use reth_provider::{
    ChainSpecProvider,
    providers::{BlockchainProvider, RocksDBProvider},
};
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{PoolConfig, PoolTransaction, TransactionPool};
use secp256k1::SecretKey;
use tracing::{info, trace, warn};

use crate::{
    BlockReaderFor, DebugNodeConfig, DebugNodeLauncher, EngineNodeLauncher, LaunchNode,
    common::WithConfigs,
    components::ComponentBuilder,
    node::FullNode,
    rpc::{RethRpcAddOns, RethRpcServerHandles, RpcContext},
};

mod states;
pub use states::*;

/// The adapter type for a reth node with the builtin provider type
// Note: we need to hardcode this because custom components might depend on it in associated types.

#[expect(clippy::doc_markdown)]
#[cfg_attr(doc, aquamarine::aquamarine)]
/// Declaratively construct a node.
///
/// [`NodeBuilder`] provides a [builder-like interface][builder] for composing
/// components of a node.
///
/// ## Order
///
/// Configuring a node starts with a [`NodeConfig`], a database, and a state provider.
/// Next the runtime components are configured:
///
///  - The EVM and Executor configuration: [`BaseEvmConfig`](base_execution_evm::BaseEvmConfig)
///  - The transaction pool: the Base transaction pool builder
///  - The network: the Base network builder
///  - The payload builder: [`PayloadBuilder`](crate::components::PayloadServiceBuilder)
///
/// Once all the components are configured, the node is ready to be launched.
///
/// On launch the builder returns a fully type aware [`NodeHandle`] that has access to all the
/// configured components and can interact with the node.
///
/// ## Components
///
/// A [`ComponentBuilder`] creates the node components during launch. Base supplies
/// its concrete component builder, which creates the pool before the network and payload service.
///
/// All builder traits are generic over the node types and are invoked with the [`BuilderContext`]
/// that gives access to internals of the that are needed to configure the components. This include
/// the original config, chain spec, the database provider and the task executor,
///
/// ## Hooks
///
/// Once all the components are configured, the builder can be used to set hooks that are run at
/// specific points in the node's lifecycle. This way custom services can be spawned before the node
/// is launched [`NodeBuilderWithComponents::on_component_initialized`], or once the rpc server(s)
/// are launched [`NodeBuilderWithComponents::on_rpc_started`]. The
/// [`NodeBuilderWithComponents::extend_rpc_modules`] can be used to inject custom rpc modules into
/// the rpc server before it is launched. See also [`RpcContext`] All hooks accept a closure that is
/// then invoked at the appropriate time in the node's launch process.
///
/// ## Flow
///
/// The [`NodeBuilder`] is intended to sit behind a CLI that provides the necessary [`NodeConfig`]
/// input: [`NodeBuilder::new`]
///
/// From there the builder is configured with the node's types, components, and hooks, then launched
/// with the [`WithLaunchContext::launch`] method. On launch all the builtin internals, such as the
/// `Database` and its providers [`BlockchainProvider`] are initialized before the configured
/// [`ComponentBuilder`] is invoked with the [`BuilderContext`] to create the transaction pool,
/// network, and payload builder components. When the RPC is configured, the corresponding hooks are
/// invoked to allow for custom rpc modules to be injected into the rpc server:
/// [`NodeBuilderWithComponents::extend_rpc_modules`]
///
/// Finally all components are created and all services are launched and a [`NodeHandle`] is
/// returned that can be used to interact with the node: [`FullNode`]
///
/// The following diagram shows the flow of the node builder from CLI to a launched node.
///
/// include_mmd!("docs/mermaid/builder.mmd")
///
/// ## Internals
///
/// The builder carries the database and provider backends through its construction phases.
/// [`FullNodeComponents`](reth_node_api::FullNodeComponents) exposes the initialized services.
/// After [`WithLaunchContext::launch`], the [`NodeHandle`] contains the running [`FullNode`].
///
/// ### Limitations
///
/// Currently the launch process is limited to ethereum nodes and requires all the components
/// specified above. It also expects beacon consensus with the ethereum engine API that is
/// configured by the builder itself during launch. This might change in the future.
///
/// [builder]: https://doc.rust-lang.org/1.0.0/style/ownership/builders.html
pub struct NodeBuilder<DB> {
    /// All settings for how the node should be configured.
    config: NodeConfig,
    /// The configured database for the node.
    database: DB,
    /// An optional [`RocksDBProvider`] to use instead of creating one during launch.
    rocksdb_provider: Option<RocksDBProvider>,
}

impl NodeBuilder<()> {
    /// Create a new [`NodeBuilder`].
    pub const fn new(config: NodeConfig) -> Self {
        Self { config, database: (), rocksdb_provider: None }
    }
}

impl<DB> NodeBuilder<DB> {
    /// Returns a reference to the node builder's config.
    pub const fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Returns a mutable reference to the node builder's config.
    pub const fn config_mut(&mut self) -> &mut NodeConfig {
        &mut self.config
    }

    /// Returns a reference to the node's database
    pub const fn db(&self) -> &DB {
        &self.database
    }

    /// Returns a mutable reference to the node's database
    pub const fn db_mut(&mut self) -> &mut DB {
        &mut self.database
    }

    /// Applies a fallible function to the builder.
    pub fn try_apply<F, R>(self, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        f(self)
    }

    /// Applies a fallible function to the builder, if the condition is `true`.
    pub fn try_apply_if<F, R>(self, cond: bool, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        if cond { f(self) } else { Ok(self) }
    }

    /// Apply a function to the builder
    pub fn apply<F>(self, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        f(self)
    }

    /// Apply a function to the builder, if the condition is `true`.
    pub fn apply_if<F>(self, cond: bool, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        if cond { f(self) } else { self }
    }
}

impl<DB> NodeBuilder<DB> {
    /// Configures the underlying database that the node will use.
    pub fn with_database<D>(self, database: D) -> NodeBuilder<D> {
        NodeBuilder { config: self.config, database, rocksdb_provider: self.rocksdb_provider }
    }

    /// Sets the [`RocksDBProvider`] to use instead of creating one during launch.
    pub fn with_rocksdb_provider(mut self, rocksdb_provider: RocksDBProvider) -> Self {
        self.rocksdb_provider = Some(rocksdb_provider);
        self
    }

    /// Preconfigure the builder with the context to launch the node.
    ///
    /// This provides the task executor and the data directory for the node.
    pub const fn with_launch_context(self, task_executor: TaskExecutor) -> WithLaunchContext<Self> {
        WithLaunchContext { builder: self, task_executor }
    }

    /// Creates an _ephemeral_ preconfigured node for testing purposes.
    #[cfg(feature = "test-utils")]
    pub fn testing_node(
        self,
        task_executor: TaskExecutor,
    ) -> WithLaunchContext<NodeBuilder<Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>>
    {
        let path = reth_db::test_utils::tempdir_path();
        self.testing_node_with_datadir(task_executor, path)
    }

    /// Creates a preconfigured node for testing purposes with a specific datadir.
    ///
    /// The entire `datadir` will be cleaned up when the node is dropped.
    #[cfg(feature = "test-utils")]
    pub fn testing_node_with_datadir(
        mut self,
        task_executor: TaskExecutor,
        datadir: impl Into<std::path::PathBuf>,
    ) -> WithLaunchContext<NodeBuilder<Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>>
    {
        let path = reth_node_core::dirs::MaybePlatformPath::<DataDirPath>::from(datadir.into());
        self.config = self.config.with_datadir_args(reth_node_core::args::DatadirArgs {
            datadir: path.clone(),
            ..Default::default()
        });

        let data_dir =
            path.unwrap_or_chain_default(self.config.chain.chain(), self.config.datadir.clone());

        let db = reth_db::test_utils::create_test_rw_db_with_datadir(data_dir.data_dir());

        WithLaunchContext { builder: self.with_database(db), task_executor }
    }
}

impl<DB> NodeBuilder<DB>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    /// Advances the state of the node builder to the next state where all components are configured
    pub fn with_components(
        self,
        components_builder: ComponentBuilder<DB>,
    ) -> NodeBuilderWithComponents<DB, ()> {
        NodeBuilderWithComponents {
            config: self.config,
            database: self.database,
            rocksdb_provider: self.rocksdb_provider,
            components_builder,
            add_ons: (),
            hooks: crate::hooks::NodeHooks::default(),
            exexs: Vec::new(),
        }
    }
}

/// A [`NodeBuilder`] with its launch context already configured.
///
/// This exposes the same methods as [`NodeBuilder`] but with the launch context already configured,
/// See [`WithLaunchContext::launch`]
pub struct WithLaunchContext<Builder> {
    builder: Builder,
    task_executor: TaskExecutor,
}

impl<Builder> WithLaunchContext<Builder> {
    /// Returns a reference to the task executor.
    pub const fn task_executor(&self) -> &TaskExecutor {
        &self.task_executor
    }
}

impl<DB> WithLaunchContext<NodeBuilder<DB>> {
    /// Returns a reference to the node builder's config.
    pub const fn config(&self) -> &NodeConfig {
        self.builder.config()
    }

    /// Returns a mutable reference to the node builder's config.
    pub const fn config_mut(&mut self) -> &mut NodeConfig {
        self.builder.config_mut()
    }
}

impl<DB> WithLaunchContext<NodeBuilder<DB>>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    /// Sets the [`RocksDBProvider`] to use instead of creating one during launch.
    pub fn with_rocksdb_provider(mut self, rocksdb_provider: RocksDBProvider) -> Self {
        self.builder.rocksdb_provider = Some(rocksdb_provider);
        self
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> WithLaunchContext<NodeBuilder<DB>> {
    /// Advances the state of the node builder to the next state where all components are configured
    pub fn with_components(
        self,
        components_builder: ComponentBuilder<DB>,
    ) -> WithLaunchContext<NodeBuilderWithComponents<DB, ()>> {
        WithLaunchContext {
            builder: self.builder.with_components(components_builder),
            task_executor: self.task_executor,
        }
    }
}

impl<DB> WithLaunchContext<NodeBuilderWithComponents<DB, ()>>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    /// Advances the state of the node builder to the next state where all customizable
    /// [`NodeAddOns`] types are configured.
    pub fn with_add_ons<AO>(
        self,
        add_ons: AO,
    ) -> WithLaunchContext<NodeBuilderWithComponents<DB, AO>>
    where
        AO: NodeAddOns<NodeAdapter<DB>>,
    {
        WithLaunchContext {
            builder: self.builder.with_add_ons(add_ons),
            task_executor: self.task_executor,
        }
    }
}

impl<DB, AO> WithLaunchContext<NodeBuilderWithComponents<DB, AO>>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    AO: RethRpcAddOns<NodeAdapter<DB>>,
{
    /// Returns a reference to the node builder's config.
    pub const fn config(&self) -> &NodeConfig {
        &self.builder.config
    }

    /// Returns a mutable reference to the node builder's config.
    pub const fn config_mut(&mut self) -> &mut NodeConfig {
        &mut self.builder.config
    }

    /// Returns a reference to node's database.
    pub const fn db(&self) -> &DB {
        &self.builder.database
    }

    /// Returns a mutable reference to node's database.
    pub const fn db_mut(&mut self) -> &mut DB {
        &mut self.builder.database
    }

    /// Applies a fallible function to the builder.
    pub fn try_apply<F, R>(self, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        f(self)
    }

    /// Applies a fallible function to the builder, if the condition is `true`.
    pub fn try_apply_if<F, R>(self, cond: bool, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        if cond { f(self) } else { Ok(self) }
    }

    /// Apply a function to the builder
    pub fn apply<F>(self, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        f(self)
    }

    /// Apply a function to the builder, if the condition is `true`.
    pub fn apply_if<F>(self, cond: bool, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        if cond { f(self) } else { self }
    }

    /// Sets the hook that is run once the node's components are initialized.
    pub fn on_component_initialized<F>(self, hook: F) -> Self
    where
        F: FnOnce(NodeAdapter<DB>) -> eyre::Result<()> + Send + 'static,
    {
        Self {
            builder: self.builder.on_component_initialized(hook),
            task_executor: self.task_executor,
        }
    }

    /// Sets the hook that is run once the node has started.
    pub fn on_node_started<F>(self, hook: F) -> Self
    where
        F: FnOnce(FullNode<NodeAdapter<DB>, AO>) -> eyre::Result<()> + Send + 'static,
    {
        Self { builder: self.builder.on_node_started(hook), task_executor: self.task_executor }
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
    pub fn map_add_ons<F>(self, f: F) -> Self
    where
        F: FnOnce(AO) -> AO,
    {
        Self { builder: self.builder.map_add_ons(f), task_executor: self.task_executor }
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
        Self { builder: self.builder.on_rpc_started(hook), task_executor: self.task_executor }
    }

    /// Sets the hook that is run to configure the rpc modules.
    ///
    /// This hook can obtain the node's components (txpool, provider, etc.) and can modify the
    /// modules that the RPC server installs.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use jsonrpsee::{core::RpcResult, proc_macros::rpc};
    ///
    /// #[derive(Clone)]
    /// struct CustomApi<Pool> { pool: Pool }
    ///
    /// #[rpc(server, namespace = "custom")]
    /// impl CustomApi {
    ///     #[method(name = "hello")]
    ///     async fn hello(&self) -> RpcResult<String> {
    ///         Ok("World".to_string())
    ///     }
    /// }
    ///
    /// let node = NodeBuilder::new(config)
    ///
    ///     .with_components(BaseNode::default().components().into_builder())
    ///     .with_add_ons(BaseNode::default().add_ons_builder().build())
    ///     .extend_rpc_modules(|ctx| {
    ///         // Access node components, so they can used by the CustomApi
    ///         let pool = ctx.pool().clone();
    ///
    ///         // Add custom RPC namespace
    ///         ctx.modules.merge_configured(CustomApi { pool }.into_rpc())?;
    ///
    ///         Ok(())
    ///     })
    ///     .build()?;
    /// ```
    pub fn extend_rpc_modules<F>(self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, NodeAdapter<DB>, AO::EthApi>) -> eyre::Result<()> + Send + 'static,
    {
        Self { builder: self.builder.extend_rpc_modules(hook), task_executor: self.task_executor }
    }

    /// Installs an `ExEx` (Execution Extension) in the node.
    ///
    /// # Note
    ///
    /// The `ExEx` ID must be unique.
    pub fn install_exex<F, R, E>(self, exex_id: impl Into<String>, exex: F) -> Self
    where
        F: FnOnce(ExExContext<NodeAdapter<DB>>) -> R + Send + 'static,
        R: Future<Output = eyre::Result<E>> + Send,
        E: Future<Output = eyre::Result<()>> + Send,
    {
        Self {
            builder: self.builder.install_exex(exex_id, exex),
            task_executor: self.task_executor,
        }
    }

    /// Installs an `ExEx` (Execution Extension) in the node if the condition is true.
    ///
    /// # Note
    ///
    /// The `ExEx` ID must be unique.
    pub fn install_exex_if<F, R, E>(self, cond: bool, exex_id: impl Into<String>, exex: F) -> Self
    where
        F: FnOnce(ExExContext<NodeAdapter<DB>>) -> R + Send + 'static,
        R: Future<Output = eyre::Result<E>> + Send,
        E: Future<Output = eyre::Result<()>> + Send,
    {
        if cond { self.install_exex(exex_id, exex) } else { self }
    }

    /// Launches the node with the given launcher.
    pub async fn launch_with<L>(self, launcher: L) -> eyre::Result<L::Node>
    where
        L: LaunchNode<NodeBuilderWithComponents<DB, AO>>,
    {
        launcher.launch_node(self.builder).await
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

    /// Launches the node with the [`EngineNodeLauncher`] that sets up engine API consensus and rpc
    pub async fn launch(
        self,
    ) -> eyre::Result<<EngineNodeLauncher as LaunchNode<NodeBuilderWithComponents<DB, AO>>>::Node>
    where
        EngineNodeLauncher: LaunchNode<NodeBuilderWithComponents<DB, AO>>,
    {
        let launcher = self.engine_api_launcher();
        self.builder.launch_with(launcher).await
    }

    /// Launches the node with the [`DebugNodeLauncher`].
    ///
    /// This is equivalent to [`WithLaunchContext::launch`], but will enable the debugging features,
    /// if they are configured.
    pub fn launch_with_debug_capabilities<R>(
        self,
        config: DebugNodeConfig<R>,
    ) -> <DebugNodeLauncher<EngineNodeLauncher, R> as LaunchNode<
        NodeBuilderWithComponents<DB, AO>,
    >>::Future
    where DebugNodeLauncher<EngineNodeLauncher, R>: LaunchNode<NodeBuilderWithComponents<DB, AO>>,
{
        let Self { builder, task_executor } = self;

        let engine_tree_config = builder.config.tree_config();

        let launcher = DebugNodeLauncher::new(
            EngineNodeLauncher::new(task_executor, builder.config.datadir(), engine_tree_config),
            config,
        );
        builder.launch_with(launcher)
    }

    /// Returns an [`EngineNodeLauncher`] that can be used to launch the node with engine API
    /// support.
    pub fn engine_api_launcher(&self) -> EngineNodeLauncher {
        let engine_tree_config = self.builder.config.tree_config();
        EngineNodeLauncher::new(
            self.task_executor.clone(),
            self.builder.config.datadir(),
            engine_tree_config,
        )
    }
}

/// Captures the necessary context for building the components of the node.
pub struct BuilderContext<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> {
    /// The current head of the blockchain at launch.
    pub(crate) head: Head,
    /// The configured provider to interact with the blockchain.
    pub(crate) provider: BlockchainProvider<DB>,
    /// The executor of the node.
    pub(crate) executor: TaskExecutor,
    /// Config container
    pub(crate) config_container: WithConfigs,
    /// Cache of recovered transaction senders shared by node components, if enabled.
    sender_recovery_cache: Option<base_execution_evm::SenderRecoveryCache>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> BuilderContext<DB> {
    /// Create a new instance of [`BuilderContext`]
    pub fn new(
        head: Head,
        provider: BlockchainProvider<DB>,
        executor: TaskExecutor,
        config_container: WithConfigs,
    ) -> Self {
        let sender_recovery_cache = config_container
            .config
            .engine
            .sender_recovery_cache_enabled
            .then(base_execution_evm::SenderRecoveryCache::default);
        Self { head, provider, executor, config_container, sender_recovery_cache }
    }

    /// Returns the configured provider to interact with the blockchain.
    pub const fn provider(&self) -> &BlockchainProvider<DB> {
        &self.provider
    }

    /// Returns the current head of the blockchain at launch.
    pub const fn head(&self) -> Head {
        self.head
    }

    /// Returns the config of the node.
    pub const fn config(&self) -> &NodeConfig {
        &self.config_container.config
    }

    /// Returns a mutable reference to the config of the node.
    pub const fn config_mut(&mut self) -> &mut NodeConfig {
        &mut self.config_container.config
    }

    /// Returns the loaded reh.toml config.
    pub const fn reth_config(&self) -> &reth_config::Config {
        &self.config_container.toml_config
    }

    /// Returns the executor of the node.
    ///
    /// This can be used to execute async tasks or functions during the setup.
    pub const fn task_executor(&self) -> &TaskExecutor {
        &self.executor
    }

    /// Returns the sender recovery cache shared by node components, if enabled.
    pub const fn sender_recovery_cache(&self) -> Option<&base_execution_evm::SenderRecoveryCache> {
        self.sender_recovery_cache.as_ref()
    }

    /// Returns the chain spec of the node.
    pub fn chain_spec(&self) -> Arc<BaseChainSpec> {
        self.provider().chain_spec()
    }

    /// Returns true if the node is configured as --dev
    pub const fn is_dev(&self) -> bool {
        self.config().dev.dev
    }

    /// Returns the transaction pool config of the node.
    pub fn pool_config(&self) -> PoolConfig {
        self.config().txpool.pool_config()
    }

    /// Loads `EnvKzgSettings::Default`.
    pub const fn kzg_settings(&self) -> eyre::Result<EnvKzgSettings> {
        Ok(EnvKzgSettings::Default)
    }

    /// Returns the config for payload building.
    pub fn payload_builder_config(&self) -> impl PayloadBuilderConfig {
        self.config().builder.clone()
    }

    /// Convenience function to start the network tasks.
    ///
    /// Spawns the configured network and associated tasks and returns the [`NetworkHandle`]
    /// connected to that network.
    pub fn start_network<Pool>(&self, builder: NetworkBuilder<(), ()>, pool: Pool) -> NetworkHandle
    where
        Pool: TransactionPool<
                Transaction: PoolTransaction<
                    Consensus = BaseTxEnvelope,
                    Pooled = base_common_consensus::BasePooledTransaction,
                >,
            > + Unpin
            + 'static,
        BlockchainProvider<DB>: BlockReaderFor,
    {
        self.start_network_with(
            builder,
            pool,
            self.config().network.transactions_manager_config(),
            self.config().network.tx_propagation_policy,
        )
    }

    /// Convenience function to start the network tasks.
    ///
    /// Accepts the config for the transaction task and the policy for propagation.
    /// Uses the default [`StrictEthAnnouncementFilter`] for announcement filtering.
    ///
    /// Spawns the configured network and associated tasks and returns the [`NetworkHandle`]
    /// connected to that network.
    pub fn start_network_with<Pool, Policy>(
        &self,
        builder: NetworkBuilder<(), ()>,
        pool: Pool,
        tx_config: TransactionsManagerConfig,
        propagation_policy: Policy,
    ) -> NetworkHandle
    where
        Pool: TransactionPool<
                Transaction: PoolTransaction<
                    Consensus = BaseTxEnvelope,
                    Pooled = base_common_consensus::BasePooledTransaction,
                >,
            > + Unpin
            + 'static,
        BlockchainProvider<DB>: BlockReaderFor,
        Policy: TransactionPropagationPolicy,
    {
        self.start_network_with_policies(
            builder,
            pool,
            tx_config,
            propagation_policy,
            StrictEthAnnouncementFilter::default(),
        )
    }

    /// Convenience function to start the network tasks with custom policies.
    ///
    /// Accepts the config for the transaction task, the policy for propagation,
    /// and a custom announcement filter. This is useful for configuring which tx types are accepted
    /// in announcements.
    ///
    /// Spawns the configured network and associated tasks and returns the [`NetworkHandle`]
    /// connected to that network.
    pub fn start_network_with_policies<Pool, PropPolicy, AnnPolicy>(
        &self,
        builder: NetworkBuilder<(), ()>,
        pool: Pool,
        tx_config: TransactionsManagerConfig,
        propagation_policy: PropPolicy,
        announcement_policy: AnnPolicy,
    ) -> NetworkHandle
    where
        Pool: TransactionPool<
                Transaction: PoolTransaction<
                    Consensus = BaseTxEnvelope,
                    Pooled = base_common_consensus::BasePooledTransaction,
                >,
            > + Unpin
            + 'static,
        BlockchainProvider<DB>: BlockReaderFor,
        PropPolicy: TransactionPropagationPolicy,
        AnnPolicy: AnnouncementFilteringPolicy,
    {
        let (handle, network, txpool, eth) = builder
            .transactions_with_policies(
                pool.clone(),
                tx_config,
                propagation_policy,
                announcement_policy,
            )
            .map_transactions(|transactions| {
                if let Some(cache) = self.sender_recovery_cache.clone() {
                    transactions.with_sender_recovery_cache(cache)
                } else {
                    transactions
                }
            })
            .request_handler_with_blob_store(self.provider().clone(), pool.blob_store())
            .split_with_handle();

        self.executor.spawn_critical_blocking_task("p2p txpool", txpool);
        self.executor.spawn_critical_blocking_task("p2p eth request handler", eth);

        let default_peers_path = self.config().datadir().known_peers();
        let known_peers_file = self.config().network.persistent_peers_file(default_peers_path);
        self.executor.spawn_critical_with_graceful_shutdown_signal(
            "p2p network task",
            |shutdown| {
                network.run_until_graceful_shutdown(shutdown, |network| {
                    if let Some(peers_file) = known_peers_file {
                        let num_known_peers = network.num_known_peers();
                        trace!(target: "reth::cli", peers_file=?peers_file, num_peers=%num_known_peers, "Saving current peers");
                        match network.write_peers_to_file(peers_file.as_path()) {
                            Ok(_) => {
                                info!(target: "reth::cli", peers_file=?peers_file, "Wrote network peers to file");
                            }
                            Err(err) => {
                                warn!(target: "reth::cli", %err, "Failed to write network peers to file");
                            }
                        }
                    }
                })
            },
        );

        handle
    }

    /// Get the network secret from the given data dir
    fn network_secret(&self, data_dir: &ChainPath<DataDirPath>) -> eyre::Result<SecretKey> {
        let secret_key = self.config().network.secret_key(data_dir.p2p_secret())?;
        Ok(secret_key)
    }

    /// Builds the [`NetworkConfig`].
    pub fn build_network_config(
        &self,
        network_builder: NetworkConfigBuilder,
    ) -> NetworkConfig<BlockchainProvider<DB>> {
        network_builder.build(self.provider.clone())
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> BuilderContext<DB> {
    /// Creates the [`NetworkBuilder`] for the node.
    pub async fn network_builder(&self) -> eyre::Result<NetworkBuilder<(), ()>> {
        let network_config = self.network_config()?;
        let builder = NetworkManager::builder(network_config).await?;
        Ok(builder)
    }

    /// Returns the default network config for the node.
    pub fn network_config(&self) -> eyre::Result<NetworkConfig<BlockchainProvider<DB>>> {
        let network_builder = self.network_config_builder();
        Ok(self.build_network_config(network_builder?))
    }

    /// Get the [`NetworkConfigBuilder`].
    pub fn network_config_builder(&self) -> eyre::Result<NetworkConfigBuilder> {
        let secret_key = self.network_secret(&self.config().datadir())?;
        let default_peers_path = self.config().datadir().known_peers();
        let builder = self
            .config()
            .network
            .network_config(
                self.reth_config(),
                &self.config().chain,
                secret_key,
                default_peers_path,
                self.executor.clone(),
            )
            .set_head(self.head);

        Ok(builder)
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> std::fmt::Debug
    for BuilderContext<DB>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuilderContext")
            .field("head", &self.head)
            .field("provider", &std::any::type_name::<BlockchainProvider<DB>>())
            .field("executor", &self.executor)
            .field("config", &self.config())
            .finish()
    }
}
