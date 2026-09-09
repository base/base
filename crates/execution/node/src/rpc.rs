//! Builder support for rpc components.

use std::{
    fmt,
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use base_execution_chainspec::ChainSpecProvider;
use base_execution_payload_builder::{BaseEngineValidator, PayloadBuilderHandle};
use base_execution_rpc::{
    AdminApi, BaseEthApiBuilder, BaseNodeEthApi, DevSigner, EthApiCtx, RpcNodeCore,
};
use base_node_context::{AddOnsContext, NodeAddOns};
pub use jsonrpsee::{
    core::middleware::layer::Either,
    server::middleware::rpc::{RpcService, RpcServiceBuilder},
};
use reth_chain_state::CanonStateSubscriptions;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_engine_primitives::TreeConfig;
pub use reth_engine_tree::tree::{BasicEngineValidator, EngineValidator};
use reth_node_core::{cli::config::RethTransactionPoolConfig, node_config::NodeConfig};
use reth_provider::providers::BlockchainProvider;
pub use reth_rpc_builder::{Identity, Stack, middleware::RethRpcMiddleware};
use reth_rpc_builder::{
    RpcModuleBuilder, RpcRegistryInner, RpcServerConfig, RpcServerHandle, TransportRpcModules,
    config::RethRpcServerConfig,
};
use reth_rpc_eth_types::{EthStateCache, cache::cache_new_blocks_task};
use reth_storage_overlay::OverlayManager;
use reth_tracing::tracing::{debug, info};

use crate::{InvalidBlockHookBuilder, TxpoolPrewarmSource};

/// Contains the handles to the spawned RPC servers.
///
/// This can be used to access the endpoints of the servers.
#[derive(Debug, Clone)]
pub struct RethRpcServerHandles {
    /// The regular RPC server handle to all configured transports.
    pub rpc: RpcServerHandle,
}

/// Contains hooks that are called during the rpc setup.
pub struct RpcHooks<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> {
    /// Hooks to run once RPC server is running.
    pub on_rpc_started: Box<dyn OnRpcStarted<DB, EthApi>>,
    /// Hooks to run to configure RPC server API.
    pub extend_rpc_modules: Box<dyn ExtendRpcModules<DB, EthApi>>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> Default
    for RpcHooks<DB, EthApi>
where
    EthApi: RpcNodeCore,
{
    fn default() -> Self {
        Self { on_rpc_started: Box::<()>::default(), extend_rpc_modules: Box::<()>::default() }
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> RpcHooks<DB, EthApi>
where
    EthApi: RpcNodeCore,
{
    /// Sets the hook that is run once the rpc server is started.
    pub(crate) fn set_on_rpc_started<F>(&mut self, hook: F) -> &mut Self
    where
        F: OnRpcStarted<DB, EthApi> + 'static,
    {
        self.on_rpc_started = Box::new(hook);
        self
    }

    /// Sets the hook that is run once the rpc server is started.
    #[expect(unused)]
    pub(crate) fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: OnRpcStarted<DB, EthApi> + 'static,
    {
        self.set_on_rpc_started(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub(crate) fn set_extend_rpc_modules<F>(&mut self, hook: F) -> &mut Self
    where
        F: ExtendRpcModules<DB, EthApi> + 'static,
    {
        self.extend_rpc_modules = Box::new(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    #[expect(unused)]
    pub(crate) fn extend_rpc_modules<F>(mut self, hook: F) -> Self
    where
        F: ExtendRpcModules<DB, EthApi> + 'static,
    {
        self.set_extend_rpc_modules(hook);
        self
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> fmt::Debug
    for RpcHooks<DB, EthApi>
where
    EthApi: RpcNodeCore,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcHooks")
            .field("on_rpc_started", &"...")
            .field("extend_rpc_modules", &"...")
            .finish()
    }
}

/// Event hook that is called once the rpc server is started.
pub trait OnRpcStarted<
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    EthApi: RpcNodeCore,
>: Send
{
    /// The hook that is called once the rpc server is started.
    fn on_rpc_started(
        self: Box<Self>,
        ctx: RpcContext<'_, DB, EthApi>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()>;
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi, F> OnRpcStarted<DB, EthApi>
    for F
where
    F: FnOnce(RpcContext<'_, DB, EthApi>, RethRpcServerHandles) -> eyre::Result<()> + Send,
    EthApi: RpcNodeCore,
{
    fn on_rpc_started(
        self: Box<Self>,
        ctx: RpcContext<'_, DB, EthApi>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()> {
        (*self)(ctx, handles)
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> OnRpcStarted<DB, EthApi>
    for ()
where
    EthApi: RpcNodeCore,
{
    fn on_rpc_started(
        self: Box<Self>,
        _: RpcContext<'_, DB, EthApi>,
        _: RethRpcServerHandles,
    ) -> eyre::Result<()> {
        Ok(())
    }
}

/// Event hook that is called when the rpc server is started.
pub trait ExtendRpcModules<
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    EthApi: RpcNodeCore,
>: Send
{
    /// The hook that is called once the rpc server is started.
    fn extend_rpc_modules(self: Box<Self>, ctx: RpcContext<'_, DB, EthApi>) -> eyre::Result<()>;
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi, F>
    ExtendRpcModules<DB, EthApi> for F
where
    F: FnOnce(RpcContext<'_, DB, EthApi>) -> eyre::Result<()> + Send,
    EthApi: RpcNodeCore,
{
    fn extend_rpc_modules(self: Box<Self>, ctx: RpcContext<'_, DB, EthApi>) -> eyre::Result<()> {
        (*self)(ctx)
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> ExtendRpcModules<DB, EthApi>
    for ()
where
    EthApi: RpcNodeCore,
{
    fn extend_rpc_modules(self: Box<Self>, _: RpcContext<'_, DB, EthApi>) -> eyre::Result<()> {
        Ok(())
    }
}

/// Helper wrapper type to encapsulate the [`RpcRegistryInner`] over components trait.
#[derive(Debug, Clone)]
#[expect(clippy::type_complexity)]
pub struct RpcRegistry<
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    EthApi: RpcNodeCore,
> {
    pub(crate) registry: RpcRegistryInner<
        BlockchainProvider<DB>,
        base_node_context::BaseNodePool<BlockchainProvider<DB>>,
        reth_network::NetworkHandle,
        EthApi,
    >,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> Deref
    for RpcRegistry<DB, EthApi>
where
    EthApi: RpcNodeCore,
{
    type Target = RpcRegistryInner<
        BlockchainProvider<DB>,
        base_node_context::BaseNodePool<BlockchainProvider<DB>>,
        reth_network::NetworkHandle,
        EthApi,
    >;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> DerefMut
    for RpcRegistry<DB, EthApi>
where
    EthApi: RpcNodeCore,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

/// Helper container for the parameters commonly passed to RPC module extension functions.
#[expect(missing_debug_implementations)]
pub struct RpcModuleContainer<
    'a,
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    EthApi: RpcNodeCore,
> {
    /// Holds installed modules per transport type.
    pub modules: &'a mut TransportRpcModules,
    /// A Helper type the holds instances of the configured modules.
    pub registry: &'a mut RpcRegistry<DB, EthApi>,
}

/// Helper container for [`RpcRegistryInner`], [`TransportRpcModules`] and
/// their lifecycle hooks.
///
/// This can be used to access installed modules, or create commonly used handlers like
/// [`base_execution_rpc::EthApi`], and ultimately merge additional rpc handler into the configured
/// transport modules [`TransportRpcModules`].
#[expect(missing_debug_implementations)]
pub struct RpcContext<
    'a,
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    EthApi: RpcNodeCore,
> {
    /// The node components.
    pub(crate) node: base_node_context::BaseNodeContext<DB>,

    /// Gives access to the node configuration.
    pub(crate) config: &'a NodeConfig,

    /// A Helper type the holds instances of the configured modules.
    ///
    /// This provides easy access to rpc handlers, such as [`RpcRegistryInner::eth_api`].
    pub registry: &'a mut RpcRegistry<DB, EthApi>,
    /// Holds installed modules per transport type.
    ///
    /// This can be used to merge additional modules into the configured HTTP and WebSocket transports. See [`TransportRpcModules::merge_configured`]
    pub modules: &'a mut TransportRpcModules,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi> RpcContext<'_, DB, EthApi>
where
    EthApi: RpcNodeCore,
{
    /// Returns the config of the node.
    pub const fn config(&self) -> &NodeConfig {
        self.config
    }

    /// Returns a reference to the configured node.
    ///
    /// This gives access to the node's components.
    pub const fn node(&self) -> &base_node_context::BaseNodeContext<DB> {
        &self.node
    }

    /// Returns the transaction pool instance.
    pub fn pool(&self) -> &base_node_context::BaseNodePool<BlockchainProvider<DB>> {
        self.node.pool()
    }

    /// Returns provider to interact with the node.
    pub fn provider(&self) -> &BlockchainProvider<DB> {
        self.node.provider()
    }

    /// Returns the handle to the network
    pub fn network(&self) -> &reth_network::NetworkHandle {
        self.node.network()
    }

    /// Returns the handle to the payload builder service
    pub fn payload_builder_handle(&self) -> &PayloadBuilderHandle {
        self.node.payload_builder_handle()
    }
}

/// Handle to the launched RPC servers.
pub struct RpcHandle<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi: RpcNodeCore>
{
    /// Handles to launched servers.
    pub rpc_server_handles: RethRpcServerHandles,
    /// Configured RPC modules.
    pub rpc_registry: RpcRegistry<DB, EthApi>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi: RpcNodeCore> Clone
    for RpcHandle<DB, EthApi>
{
    fn clone(&self) -> Self {
        Self {
            rpc_server_handles: self.rpc_server_handles.clone(),
            rpc_registry: self.rpc_registry.clone(),
        }
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi: RpcNodeCore> Deref
    for RpcHandle<DB, EthApi>
{
    type Target = RpcRegistry<DB, EthApi>;

    fn deref(&self) -> &Self::Target {
        &self.rpc_registry
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi: RpcNodeCore> Debug
    for RpcHandle<DB, EthApi>
where
    RpcRegistry<DB, EthApi>: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcHandle")
            .field("rpc_server_handles", &self.rpc_server_handles)
            .field("rpc_registry", &self.rpc_registry)
            .finish()
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi: RpcNodeCore>
    RpcHandle<DB, EthApi>
{
    /// Returns the RPC server handles.
    pub const fn rpc_server_handles(&self) -> &RethRpcServerHandles {
        &self.rpc_server_handles
    }

    /// Returns the `EthApi` instance of the rpc server.
    pub const fn eth_api(&self) -> &EthApi {
        self.rpc_registry.registry.eth_api()
    }

    /// Returns an instance of the [`AdminApi`] for the rpc server.
    pub fn admin_api(
        &self,
    ) -> AdminApi<
        reth_network::NetworkHandle,
        base_node_context::BaseNodePool<BlockchainProvider<DB>>,
    > {
        self.rpc_registry.registry.admin_api()
    }
}

/// Prepared public RPC modules and lifecycle hooks.
pub struct RpcSetupContext<
    'a,
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    EthApi: RpcNodeCore,
> {
    pub node: base_node_context::BaseNodeContext<DB>,
    pub config: &'a NodeConfig,
    pub modules: TransportRpcModules,
    pub registry: RpcRegistry<DB, EthApi>,
    pub on_rpc_started: Box<dyn OnRpcStarted<DB, EthApi>>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, EthApi: RpcNodeCore> fmt::Debug
    for RpcSetupContext<'_, DB, EthApi>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcSetupContext").field("modules", &self.modules).finish_non_exhaustive()
    }
}

/// Node add-ons containing RPC server configuration, with customizable eth API handler.
///
/// This struct can be used to provide the RPC server functionality. It is responsible for launching
/// the public HTTP and WebSocket RPC servers. It is intended to be used and
/// modified as part of the [`NodeAddOns`] see for example `OpRpcAddons`, `EthereumAddOns`.
///
/// It can be modified to register RPC API handlers, see [`RpcAddOns::launch_add_ons_with`] which
/// takes a closure that provides access to all the configured modules (namespaces), and is invoked
/// just before the servers are launched. This can be used to extend the node with custom RPC
/// methods or even replace existing method handlers, see also [`TransportRpcModules`].
pub struct RpcAddOns<
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    RpcMiddleware = Identity,
> {
    /// Additional RPC add-ons.
    pub hooks: RpcHooks<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>,
    /// Builder for `EthApi`
    eth_api_builder: BaseEthApiBuilder,

    /// Configurable RPC middleware stack.
    ///
    /// This middleware is applied to all RPC requests across all transports (HTTP, WS, IPC).
    /// See [`RpcAddOns::with_rpc_middleware`] for more details.
    rpc_middleware: RpcMiddleware,
    /// Optional custom tokio runtime for the RPC server.
    tokio_runtime: Option<tokio::runtime::Handle>,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, RpcMiddleware> Debug
    for RpcAddOns<DB, RpcMiddleware>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcAddOns")
            .field("hooks", &self.hooks)
            .field("eth_api_builder", &"...")
            .field("rpc_middleware", &"...")
            .finish()
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, RpcMiddleware>
    RpcAddOns<DB, RpcMiddleware>
{
    /// Creates a new instance of the RPC add-ons.
    pub fn new(eth_api_builder: BaseEthApiBuilder, rpc_middleware: RpcMiddleware) -> Self {
        Self { hooks: RpcHooks::default(), eth_api_builder, rpc_middleware, tokio_runtime: None }
    }

    /// Sets the RPC middleware stack for processing RPC requests.
    ///
    /// This method configures a custom middleware stack that will be applied to all RPC requests
    /// across HTTP, `WebSocket`, and IPC transports. The middleware is applied to the RPC service
    /// layer, allowing you to intercept, modify, or enhance RPC request processing.
    ///
    ///
    /// # How It Works
    ///
    /// The middleware uses the Tower ecosystem's `Layer` pattern. When an RPC server is started,
    /// the configured middleware stack is applied to create a layered service that processes
    /// requests in the order the layers were added.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use reth_rpc_builder::{RpcServiceBuilder, RpcRequestMetrics};
    /// use tower::Layer;
    ///
    /// // Simple example with metrics
    /// let metrics_layer = RpcRequestMetrics::new(metrics_recorder);
    /// let with_metrics = rpc_addons.with_rpc_middleware(
    ///     RpcServiceBuilder::new().layer(metrics_layer)
    /// );
    ///
    /// // Composing multiple middleware layers
    /// let middleware_stack = RpcServiceBuilder::new()
    ///     .layer(rate_limit_layer)
    ///     .layer(logging_layer)
    ///     .layer(metrics_layer);
    /// let with_full_stack = rpc_addons.with_rpc_middleware(middleware_stack);
    /// ```
    ///
    /// # Notes
    ///
    /// - Middleware is applied to the RPC service layer, not the HTTP transport layer
    /// - The default middleware is `Identity` (no-op), which passes through requests unchanged
    /// - Middleware layers are applied in the order they are added via `.layer()`
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> RpcAddOns<DB, T> {
        let Self { hooks, eth_api_builder, tokio_runtime, .. } = self;
        RpcAddOns { hooks, eth_api_builder, rpc_middleware, tokio_runtime }
    }

    /// Sets the tokio runtime for the RPC servers.
    ///
    /// Caution: This runtime must not be created from within asynchronous context.
    pub fn with_tokio_runtime(self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        let Self { hooks, eth_api_builder, rpc_middleware, .. } = self;
        Self { hooks, eth_api_builder, rpc_middleware, tokio_runtime }
    }

    /// Add a new layer `T` to the configured [`RpcServiceBuilder`].
    pub fn layer_rpc_middleware<T>(self, layer: T) -> RpcAddOns<DB, Stack<RpcMiddleware, T>> {
        let Self { hooks, eth_api_builder, rpc_middleware, tokio_runtime } = self;
        let rpc_middleware = Stack::new(rpc_middleware, layer);
        RpcAddOns { hooks, eth_api_builder, rpc_middleware, tokio_runtime }
    }

    /// Optionally adds a new layer `T` to the configured [`RpcServiceBuilder`].
    #[expect(clippy::type_complexity)]
    pub fn option_layer_rpc_middleware<T>(
        self,
        layer: Option<T>,
    ) -> RpcAddOns<DB, Stack<RpcMiddleware, Either<T, Identity>>> {
        let layer = layer.map(Either::Left).unwrap_or(Either::Right(Identity::new()));
        self.layer_rpc_middleware(layer)
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>,
                RethRpcServerHandles,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.hooks.set_on_rpc_started(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub fn extend_rpc_modules<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.hooks.set_extend_rpc_modules(hook);
        self
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> Default for RpcAddOns<DB, Identity> {
    fn default() -> Self {
        Self::new(BaseEthApiBuilder::default(), Default::default())
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, RpcMiddleware>
    RpcAddOns<DB, RpcMiddleware>
where
    RpcMiddleware: RethRpcMiddleware,
{
    /// Launches public RPC and invokes the configured extension and lifecycle hooks.
    pub async fn launch_add_ons_with<F>(
        self,
        ctx: AddOnsContext<'_, DB>,
        ext: F,
    ) -> eyre::Result<RpcHandle<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>>
    where
        F: FnOnce(
            RpcModuleContainer<'_, DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>,
        ) -> eyre::Result<()>,
    {
        let rpc_middleware = self.rpc_middleware.clone();
        let tokio_runtime = self.tokio_runtime.clone();
        let mut setup = self.setup_rpc_components(ctx, ext).await?;
        let server_config = setup
            .config
            .rpc
            .rpc_server_config()
            .set_rpc_middleware(rpc_middleware)
            .with_tokio_runtime(tokio_runtime);
        let rpc = Self::launch_rpc_server_internal(server_config, &setup.modules).await?;
        let handles = RethRpcServerHandles { rpc };
        setup.on_rpc_started.on_rpc_started(
            RpcContext {
                node: setup.node,
                config: setup.config,
                registry: &mut setup.registry,
                modules: &mut setup.modules,
            },
            handles.clone(),
        )?;
        Ok(RpcHandle { rpc_server_handles: handles, rpc_registry: setup.registry })
    }

    /// Common setup for RPC server initialization
    async fn setup_rpc_components<'a, F>(
        self,
        ctx: AddOnsContext<'a, DB>,
        ext: F,
    ) -> eyre::Result<RpcSetupContext<'a, DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>>
    where
        F: FnOnce(
            RpcModuleContainer<'_, DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>,
        ) -> eyre::Result<()>,
    {
        let Self { eth_api_builder, hooks, .. } = self;

        let AddOnsContext { node, config, beacon_engine_handle, engine_events } = ctx;

        let cache = EthStateCache::spawn_with(
            node.provider().clone(),
            config.rpc.eth_config().cache,
            node.task_executor().clone(),
        );

        let new_canonical_blocks = node.provider().canonical_state_stream();
        let c = cache.clone();
        node.task_executor().spawn_critical_task("cache canonical blocks task", async move {
            cache_new_blocks_task(c, new_canonical_blocks).await;
        });

        let eth_config = config.rpc.eth_config().max_batch_size(config.txpool.max_batch_size());
        let ctx = EthApiCtx {
            components: &node,
            config: eth_config,
            cache,
            engine_handle: beacon_engine_handle.clone(),
        };
        let eth_api = eth_api_builder.build_eth_api(ctx).await?;

        let module_config = config.rpc.transport_rpc_module_config();
        debug!(target: "reth::cli", http=?module_config.http(), ws=?module_config.ws(), "Using RPC module config");

        let mut registry = RpcModuleBuilder::default()
            .with_provider(node.provider().clone())
            .with_pool(node.pool().clone())
            .with_network(node.network().clone())
            .with_executor(node.task_executor().clone())
            .with_evm_config(node.evm_config().clone())
            .with_consensus(node.consensus().clone())
            .into_registry(
                module_config.config().cloned().unwrap_or_default(),
                eth_api,
                engine_events,
            );
        let mut modules = registry.create_transport_rpc_modules(module_config);

        // in dev mode we generate 20 random dev-signer accounts
        if config.dev.dev {
            let signers = DevSigner::from_mnemonic(config.dev.dev_mnemonic.as_str(), 20);
            registry.eth_api().signers().write().extend(signers);
        }

        let mut registry = RpcRegistry { registry };
        let ctx = RpcContext {
            node: node.clone(),
            config,
            registry: &mut registry,
            modules: &mut modules,
        };

        let RpcHooks { on_rpc_started, extend_rpc_modules } = hooks;

        ext(RpcModuleContainer { modules: ctx.modules, registry: ctx.registry })?;
        extend_rpc_modules.extend_rpc_modules(ctx)?;

        Ok(RpcSetupContext { node, config, modules, registry, on_rpc_started })
    }

    /// Helper to launch the RPC server
    async fn launch_rpc_server_internal<M>(
        server_config: RpcServerConfig<M>,
        modules: &TransportRpcModules,
    ) -> eyre::Result<RpcServerHandle>
    where
        M: RethRpcMiddleware,
    {
        let handle = server_config.start(modules).await?;

        if let Some(addr) = handle.http_local_addr() {
            info!(target: "reth::cli", url=%addr, "RPC HTTP server started");
        }
        if let Some(addr) = handle.ws_local_addr() {
            info!(target: "reth::cli", url=%addr, "RPC WS server started");
        }

        Ok(handle)
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, RpcMiddleware> NodeAddOns<DB>
    for RpcAddOns<DB, RpcMiddleware>
where
    RpcMiddleware: RethRpcMiddleware,
{
    type Handle = RpcHandle<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>;

    async fn launch_add_ons(self, ctx: AddOnsContext<'_, DB>) -> eyre::Result<Self::Handle> {
        self.launch_add_ons_with(ctx, |_| Ok(())).await
    }
}

/// Helper trait implemented for add-ons producing [`RpcHandle`]. Used by common node launcher
/// implementations.
pub trait RethRpcAddOns<DB: Database + DatabaseMetrics + Clone + Unpin + 'static>:
    NodeAddOns<DB, Handle = RpcHandle<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>>
{
    /// Returns a mutable reference to RPC hooks.
    fn hooks_mut(
        &mut self,
    ) -> &mut RpcHooks<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>;
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, RpcMiddleware> RethRpcAddOns<DB>
    for RpcAddOns<DB, RpcMiddleware>
where
    Self: NodeAddOns<
            DB,
            Handle = RpcHandle<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>>,
        >,
{
    fn hooks_mut(
        &mut self,
    ) -> &mut RpcHooks<DB, BaseNodeEthApi<base_node_context::BaseNodeContext<DB>>> {
        &mut self.hooks
    }
}

/// Constructs the Base execution validator and its caches.
#[derive(Debug, Default, Clone)]
pub struct BasicEngineValidatorBuilder;

impl BasicEngineValidatorBuilder {
    /// Constructs the Base execution validator and its caches.
    pub async fn build_tree_validator<DB: Database + DatabaseMetrics + Clone + Unpin + 'static>(
        ctx: &AddOnsContext<'_, DB>,
        tree_config: TreeConfig,
        overlay_manager: OverlayManager,
    ) -> eyre::Result<BasicEngineValidator<BlockchainProvider<DB>>> {
        let validator = BaseEngineValidator::new(Arc::clone(&ctx.config.chain));
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = InvalidBlockHookBuilder::build(
            ctx.config,
            &data_dir,
            ctx.node.provider().clone(),
            ctx.node.evm_config().clone(),
            ctx.node.provider().chain_spec().chain().id(),
        )
        .await?;

        let txpool_prewarming = tree_config.txpool_prewarming();
        let mut validator = BasicEngineValidator::new(
            ctx.node.provider().clone(),
            ctx.node.consensus().clone(),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            overlay_manager,
            ctx.node.task_executor().clone(),
        );

        if txpool_prewarming {
            validator =
                validator.with_txpool_prewarming(TxpoolPrewarmSource::new(ctx.node.pool().clone()));
        }

        Ok(validator)
    }
}
