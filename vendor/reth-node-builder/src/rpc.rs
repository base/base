//! Builder support for rpc components.

use std::{
    fmt::{self, Debug},
    future::Future,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use base_execution_chainspec::ChainSpecProvider;
use base_execution_payload_builder::BaseEngineValidator;
use base_execution_rpc::eth::{BaseEthApiBuilder, BaseNodeEthApi, EthApiCtx};
pub use jsonrpsee::{
    core::middleware::layer::Either,
    server::middleware::rpc::{RpcService, RpcServiceBuilder},
};
use parking_lot::Mutex;
use reth_chain_state::CanonStateSubscriptions;
pub use reth_engine_tree::tree::{BasicEngineValidator, EngineValidator};
use reth_node_api::{AddOnsContext, FullNodeComponents, NodeAddOns, TreeConfig};
use reth_node_core::{cli::config::RethTransactionPoolConfig, node_config::NodeConfig};
use reth_payload_builder::PayloadBuilderHandle;
use reth_rpc::{
    AdminApi,
    eth::{DevSigner, EthApiTypes, FullEthApiServer},
};
use reth_rpc_api::eth::helpers::EthTransactions;
pub use reth_rpc_builder::{
    Identity, Stack,
    middleware::{RethAuthHttpMiddleware, RethRpcMiddleware},
};
use reth_rpc_builder::{
    RpcModuleBuilder, RpcRegistryInner, RpcServerConfig, RpcServerHandle, TransportRpcModules,
    auth::{AuthRpcModule, AuthServerHandle},
    config::RethRpcServerConfig,
};
use reth_rpc_eth_types::{EthStateCache, cache::cache_new_blocks_task};
use reth_storage_overlay::OverlayManager;
use reth_tokio_util::EventSender;
use reth_tracing::tracing::{debug, info};
use reth_trie_common::KeccakKeyHasher;
use tokio::sync::oneshot;

use crate::{
    BaseEngineApiBuilder, ConsensusEngineEvent, ConsensusEngineHandle, InvalidBlockHookBuilder,
    txpool_prewarm,
};

/// Contains the handles to the spawned RPC servers.
///
/// This can be used to access the endpoints of the servers.
#[derive(Debug, Clone)]
pub struct RethRpcServerHandles {
    /// The regular RPC server handle to all configured transports.
    pub rpc: RpcServerHandle,
    /// The handle to the auth server (engine API)
    pub auth: AuthServerHandle,
}

/// Contains hooks that are called during the rpc setup.
pub struct RpcHooks<Node: FullNodeComponents, EthApi> {
    /// Hooks to run once RPC server is running.
    pub on_rpc_started: Box<dyn OnRpcStarted<Node, EthApi>>,
    /// Hooks to run to configure RPC server API.
    pub extend_rpc_modules: Box<dyn ExtendRpcModules<Node, EthApi>>,
}

impl<Node, EthApi> Default for RpcHooks<Node, EthApi>
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn default() -> Self {
        Self { on_rpc_started: Box::<()>::default(), extend_rpc_modules: Box::<()>::default() }
    }
}

impl<Node, EthApi> RpcHooks<Node, EthApi>
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    /// Sets the hook that is run once the rpc server is started.
    pub(crate) fn set_on_rpc_started<F>(&mut self, hook: F) -> &mut Self
    where
        F: OnRpcStarted<Node, EthApi> + 'static,
    {
        self.on_rpc_started = Box::new(hook);
        self
    }

    /// Sets the hook that is run once the rpc server is started.
    #[expect(unused)]
    pub(crate) fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: OnRpcStarted<Node, EthApi> + 'static,
    {
        self.set_on_rpc_started(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub(crate) fn set_extend_rpc_modules<F>(&mut self, hook: F) -> &mut Self
    where
        F: ExtendRpcModules<Node, EthApi> + 'static,
    {
        self.extend_rpc_modules = Box::new(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    #[expect(unused)]
    pub(crate) fn extend_rpc_modules<F>(mut self, hook: F) -> Self
    where
        F: ExtendRpcModules<Node, EthApi> + 'static,
    {
        self.set_extend_rpc_modules(hook);
        self
    }
}

impl<Node, EthApi> fmt::Debug for RpcHooks<Node, EthApi>
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcHooks")
            .field("on_rpc_started", &"...")
            .field("extend_rpc_modules", &"...")
            .finish()
    }
}

/// Event hook that is called once the rpc server is started.
pub trait OnRpcStarted<Node: FullNodeComponents, EthApi: EthApiTypes>: Send {
    /// The hook that is called once the rpc server is started.
    fn on_rpc_started(
        self: Box<Self>,
        ctx: RpcContext<'_, Node, EthApi>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()>;
}

impl<Node, EthApi, F> OnRpcStarted<Node, EthApi> for F
where
    F: FnOnce(RpcContext<'_, Node, EthApi>, RethRpcServerHandles) -> eyre::Result<()> + Send,
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn on_rpc_started(
        self: Box<Self>,
        ctx: RpcContext<'_, Node, EthApi>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()> {
        (*self)(ctx, handles)
    }
}

impl<Node, EthApi> OnRpcStarted<Node, EthApi> for ()
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn on_rpc_started(
        self: Box<Self>,
        _: RpcContext<'_, Node, EthApi>,
        _: RethRpcServerHandles,
    ) -> eyre::Result<()> {
        Ok(())
    }
}

/// Event hook that is called when the rpc server is started.
pub trait ExtendRpcModules<Node: FullNodeComponents, EthApi: EthApiTypes>: Send {
    /// The hook that is called once the rpc server is started.
    fn extend_rpc_modules(self: Box<Self>, ctx: RpcContext<'_, Node, EthApi>) -> eyre::Result<()>;
}

impl<Node, EthApi, F> ExtendRpcModules<Node, EthApi> for F
where
    F: FnOnce(RpcContext<'_, Node, EthApi>) -> eyre::Result<()> + Send,
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn extend_rpc_modules(self: Box<Self>, ctx: RpcContext<'_, Node, EthApi>) -> eyre::Result<()> {
        (*self)(ctx)
    }
}

impl<Node, EthApi> ExtendRpcModules<Node, EthApi> for ()
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn extend_rpc_modules(self: Box<Self>, _: RpcContext<'_, Node, EthApi>) -> eyre::Result<()> {
        Ok(())
    }
}

/// Helper wrapper type to encapsulate the [`RpcRegistryInner`] over components trait.
#[derive(Debug, Clone)]
#[expect(clippy::type_complexity)]
pub struct RpcRegistry<Node: FullNodeComponents, EthApi: EthApiTypes> {
    pub(crate) registry: RpcRegistryInner<
        Node::Provider,
        reth_node_api::BaseNodePool<Node::Provider>,
        reth_network::NetworkHandle,
        EthApi,
    >,
}

impl<Node, EthApi> Deref for RpcRegistry<Node, EthApi>
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    type Target = RpcRegistryInner<
        Node::Provider,
        reth_node_api::BaseNodePool<Node::Provider>,
        reth_network::NetworkHandle,
        EthApi,
    >;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl<Node, EthApi> DerefMut for RpcRegistry<Node, EthApi>
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

/// Helper container for the parameters commonly passed to RPC module extension functions.
#[expect(missing_debug_implementations)]
pub struct RpcModuleContainer<'a, Node: FullNodeComponents, EthApi: EthApiTypes> {
    /// Holds installed modules per transport type.
    pub modules: &'a mut TransportRpcModules,
    /// Holds jwt authenticated rpc module.
    pub auth_module: &'a mut AuthRpcModule,
    /// A Helper type the holds instances of the configured modules.
    pub registry: &'a mut RpcRegistry<Node, EthApi>,
}

/// Helper container to encapsulate [`RpcRegistryInner`], [`TransportRpcModules`] and
/// [`AuthRpcModule`].
///
/// This can be used to access installed modules, or create commonly used handlers like
/// [`reth_rpc::eth::EthApi`], and ultimately merge additional rpc handler into the configured
/// transport modules [`TransportRpcModules`] as well as configured authenticated methods
/// [`AuthRpcModule`].
#[expect(missing_debug_implementations)]
pub struct RpcContext<'a, Node: FullNodeComponents, EthApi: EthApiTypes> {
    /// The node components.
    pub(crate) node: Node,

    /// Gives access to the node configuration.
    pub(crate) config: &'a NodeConfig,

    /// A Helper type the holds instances of the configured modules.
    ///
    /// This provides easy access to rpc handlers, such as [`RpcRegistryInner::eth_api`].
    pub registry: &'a mut RpcRegistry<Node, EthApi>,
    /// Holds installed modules per transport type.
    ///
    /// This can be used to merge additional modules into the configured transports (http, ipc,
    /// ws). See [`TransportRpcModules::merge_configured`]
    pub modules: &'a mut TransportRpcModules,
    /// Holds jwt authenticated rpc module.
    ///
    /// This can be used to merge additional modules into the configured authenticated methods
    pub auth_module: &'a mut AuthRpcModule,
}

impl<Node, EthApi> RpcContext<'_, Node, EthApi>
where
    Node: FullNodeComponents,
    EthApi: EthApiTypes,
{
    /// Returns the config of the node.
    pub const fn config(&self) -> &NodeConfig {
        self.config
    }

    /// Returns a reference to the configured node.
    ///
    /// This gives access to the node's components.
    pub const fn node(&self) -> &Node {
        &self.node
    }

    /// Returns the transaction pool instance.
    pub fn pool(&self) -> &reth_node_api::BaseNodePool<Node::Provider> {
        self.node.pool()
    }

    /// Returns provider to interact with the node.
    pub fn provider(&self) -> &Node::Provider {
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
pub struct RpcHandle<Node: FullNodeComponents, EthApi: EthApiTypes> {
    /// Handles to launched servers.
    pub rpc_server_handles: RethRpcServerHandles,
    /// Configured RPC modules.
    pub rpc_registry: RpcRegistry<Node, EthApi>,
    /// Notification channel for engine API events
    ///
    /// Caution: This is a multi-producer, multi-consumer broadcast and allows grants access to
    /// dispatch events
    pub engine_events: EventSender<ConsensusEngineEvent>,
    /// Handle to the beacon consensus engine.
    pub beacon_engine_handle: ConsensusEngineHandle,
    /// Handle to trigger engine shutdown.
    pub engine_shutdown: EngineShutdown,
}

impl<Node: FullNodeComponents, EthApi: EthApiTypes> Clone for RpcHandle<Node, EthApi> {
    fn clone(&self) -> Self {
        Self {
            rpc_server_handles: self.rpc_server_handles.clone(),
            rpc_registry: self.rpc_registry.clone(),
            engine_events: self.engine_events.clone(),
            beacon_engine_handle: self.beacon_engine_handle.clone(),
            engine_shutdown: self.engine_shutdown.clone(),
        }
    }
}

impl<Node: FullNodeComponents, EthApi: EthApiTypes> Deref for RpcHandle<Node, EthApi> {
    type Target = RpcRegistry<Node, EthApi>;

    fn deref(&self) -> &Self::Target {
        &self.rpc_registry
    }
}

impl<Node: FullNodeComponents, EthApi: EthApiTypes> Debug for RpcHandle<Node, EthApi>
where
    RpcRegistry<Node, EthApi>: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcHandle")
            .field("rpc_server_handles", &self.rpc_server_handles)
            .field("rpc_registry", &self.rpc_registry)
            .field("engine_shutdown", &self.engine_shutdown)
            .finish()
    }
}

impl<Node: FullNodeComponents, EthApi: EthApiTypes> RpcHandle<Node, EthApi> {
    /// Returns the RPC server handles.
    pub const fn rpc_server_handles(&self) -> &RethRpcServerHandles {
        &self.rpc_server_handles
    }

    /// Returns the consensus engine handle.
    ///
    /// This handle can be used to interact with the engine service directly.
    pub const fn consensus_engine_handle(&self) -> &ConsensusEngineHandle {
        &self.beacon_engine_handle
    }

    /// Returns the consensus engine events sender.
    pub const fn consensus_engine_events(&self) -> &EventSender<ConsensusEngineEvent> {
        &self.engine_events
    }

    /// Returns the `EthApi` instance of the rpc server.
    pub const fn eth_api(&self) -> &EthApi {
        self.rpc_registry.registry.eth_api()
    }

    /// Returns an instance of the [`AdminApi`] for the rpc server.
    pub fn admin_api(
        &self,
    ) -> AdminApi<reth_network::NetworkHandle, reth_node_api::BaseNodePool<Node::Provider>> {
        self.rpc_registry.registry.admin_api()
    }
}

/// Handle returned when only the regular RPC server (HTTP/WS/IPC) is launched.
///
/// This handle provides access to the RPC server endpoints and registry, but does not
/// include an authenticated Engine API server. Use this when you only need regular
/// RPC functionality.
#[derive(Debug, Clone)]
pub struct RpcServerOnlyHandle<Node: FullNodeComponents, EthApi: EthApiTypes> {
    /// Handle to the RPC server
    pub rpc_server_handle: RpcServerHandle,
    /// Configured RPC modules.
    pub rpc_registry: RpcRegistry<Node, EthApi>,
    /// Notification channel for engine API events
    pub engine_events: EventSender<ConsensusEngineEvent>,
    /// Handle to the consensus engine.
    pub engine_handle: ConsensusEngineHandle,
}

impl<Node: FullNodeComponents, EthApi: EthApiTypes> RpcServerOnlyHandle<Node, EthApi> {
    /// Returns the RPC server handle.
    pub const fn rpc_server_handle(&self) -> &RpcServerHandle {
        &self.rpc_server_handle
    }

    /// Returns the consensus engine handle.
    ///
    /// This handle can be used to interact with the engine service directly.
    pub const fn consensus_engine_handle(&self) -> &ConsensusEngineHandle {
        &self.engine_handle
    }

    /// Returns the consensus engine events sender.
    pub const fn consensus_engine_events(&self) -> &EventSender<ConsensusEngineEvent> {
        &self.engine_events
    }
}

/// Handle returned when only the authenticated Engine API server is launched.
///
/// This handle provides access to the Engine API server and registry, but does not
/// include the regular RPC servers (HTTP/WS/IPC). Use this for specialized setups
/// that only need Engine API functionality.
#[derive(Debug, Clone)]
pub struct AuthServerOnlyHandle<Node: FullNodeComponents, EthApi: EthApiTypes> {
    /// Handle to the auth server (engine API)
    pub auth_server_handle: AuthServerHandle,
    /// Configured RPC modules.
    pub rpc_registry: RpcRegistry<Node, EthApi>,
    /// Notification channel for engine API events
    pub engine_events: EventSender<ConsensusEngineEvent>,
    /// Handle to the consensus engine.
    pub engine_handle: ConsensusEngineHandle,
}

impl<Node: FullNodeComponents, EthApi: EthApiTypes> AuthServerOnlyHandle<Node, EthApi> {
    /// Returns the consensus engine handle.
    ///
    /// This handle can be used to interact with the engine service directly.
    pub const fn consensus_engine_handle(&self) -> &ConsensusEngineHandle {
        &self.engine_handle
    }

    /// Returns the consensus engine events sender.
    pub const fn consensus_engine_events(&self) -> &EventSender<ConsensusEngineEvent> {
        &self.engine_events
    }
}

/// Internal context struct for RPC setup shared between different launch methods
struct RpcSetupContext<'a, Node: FullNodeComponents, EthApi: EthApiTypes> {
    node: Node,
    config: &'a NodeConfig,
    modules: TransportRpcModules,
    auth_module: AuthRpcModule,
    auth_config: reth_rpc_builder::auth::AuthServerConfig,
    registry: RpcRegistry<Node, EthApi>,
    on_rpc_started: Box<dyn OnRpcStarted<Node, EthApi>>,
    engine_events: EventSender<ConsensusEngineEvent>,
    engine_handle: ConsensusEngineHandle,
}

/// Node add-ons containing RPC server configuration, with customizable eth API handler.
///
/// This struct can be used to provide the RPC server functionality. It is responsible for launching
/// the regular RPC and the authenticated RPC server (engine API). It is intended to be used and
/// modified as part of the [`NodeAddOns`] see for example `OpRpcAddons`, `EthereumAddOns`.
///
/// It can be modified to register RPC API handlers, see [`RpcAddOns::launch_add_ons_with`] which
/// takes a closure that provides access to all the configured modules (namespaces), and is invoked
/// just before the servers are launched. This can be used to extend the node with custom RPC
/// methods or even replace existing method handlers, see also [`TransportRpcModules`].
pub struct RpcAddOns<
    Node: FullNodeComponents,
    RpcMiddleware = Identity,
    AuthHttpMiddleware = Identity,
> {
    /// Additional RPC add-ons.
    pub hooks: RpcHooks<Node, BaseNodeEthApi<Node>>,
    /// Builder for `EthApi`
    eth_api_builder: BaseEthApiBuilder,

    /// Configurable RPC middleware stack.
    ///
    /// This middleware is applied to all RPC requests across all transports (HTTP, WS, IPC).
    /// See [`RpcAddOns::with_rpc_middleware`] for more details.
    rpc_middleware: RpcMiddleware,
    /// Configurable HTTP transport middleware for the auth server.
    ///
    /// This middleware is applied after JWT authentication and before JSON-RPC parsing on the
    /// auth / Engine API server, giving access to the raw HTTP request.
    auth_http_middleware: AuthHttpMiddleware,
    /// Optional custom tokio runtime for the RPC server.
    tokio_runtime: Option<tokio::runtime::Handle>,
}

impl<Node, RpcMiddleware, AuthHttpMiddleware> Debug
    for RpcAddOns<Node, RpcMiddleware, AuthHttpMiddleware>
where
    Node: FullNodeComponents,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcAddOns")
            .field("hooks", &self.hooks)
            .field("eth_api_builder", &"...")
            .field("rpc_middleware", &"...")
            .finish()
    }
}

impl<Node, RpcMiddleware, AuthHttpMiddleware> RpcAddOns<Node, RpcMiddleware, AuthHttpMiddleware>
where
    Node: FullNodeComponents,
    BaseNodeEthApi<Node>: FullEthApiServer<
            Provider = Node::Provider,
            Pool = reth_node_api::BaseNodePool<Node::Provider>,
        >,
{
    /// Creates a new instance of the RPC add-ons.
    pub fn new(
        eth_api_builder: BaseEthApiBuilder,

        rpc_middleware: RpcMiddleware,
        auth_http_middleware: AuthHttpMiddleware,
    ) -> Self {
        Self {
            hooks: RpcHooks::default(),
            eth_api_builder,

            rpc_middleware,
            auth_http_middleware,
            tokio_runtime: None,
        }
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
    pub fn with_rpc_middleware<T>(
        self,
        rpc_middleware: T,
    ) -> RpcAddOns<Node, T, AuthHttpMiddleware> {
        let Self { hooks, eth_api_builder, auth_http_middleware, tokio_runtime, .. } = self;
        RpcAddOns { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime }
    }

    /// Configures the HTTP transport middleware for the auth / Engine API server.
    ///
    /// This middleware is applied after JWT authentication and before JSON-RPC parsing,
    /// giving access to the raw HTTP request (headers, body, etc.).
    pub fn with_auth_http_middleware<T>(
        self,
        auth_http_middleware: T,
    ) -> RpcAddOns<Node, RpcMiddleware, T> {
        let Self { hooks, eth_api_builder, rpc_middleware, tokio_runtime, .. } = self;
        RpcAddOns { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime }
    }

    /// Stacks an additional HTTP transport middleware layer for the auth / Engine API server.
    pub fn layer_auth_http_middleware<T>(
        self,
        layer: T,
    ) -> RpcAddOns<Node, RpcMiddleware, Stack<AuthHttpMiddleware, T>> {
        let Self { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime } =
            self;
        let auth_http_middleware = Stack::new(auth_http_middleware, layer);
        RpcAddOns { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime }
    }

    /// Conditionally stacks an HTTP transport middleware layer for the auth / Engine API server.
    #[expect(clippy::type_complexity)]
    pub fn option_layer_auth_http_middleware<T>(
        self,
        layer: Option<T>,
    ) -> RpcAddOns<Node, RpcMiddleware, Stack<AuthHttpMiddleware, Either<T, Identity>>> {
        let layer = layer.map(Either::Left).unwrap_or(Either::Right(Identity::new()));
        self.layer_auth_http_middleware(layer)
    }

    /// Sets the tokio runtime for the RPC servers.
    ///
    /// Caution: This runtime must not be created from within asynchronous context.
    pub fn with_tokio_runtime(self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        let Self { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, .. } = self;
        Self { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime }
    }

    /// Add a new layer `T` to the configured [`RpcServiceBuilder`].
    pub fn layer_rpc_middleware<T>(
        self,
        layer: T,
    ) -> RpcAddOns<Node, Stack<RpcMiddleware, T>, AuthHttpMiddleware> {
        let Self { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime } =
            self;
        let rpc_middleware = Stack::new(rpc_middleware, layer);
        RpcAddOns { hooks, eth_api_builder, rpc_middleware, auth_http_middleware, tokio_runtime }
    }

    /// Optionally adds a new layer `T` to the configured [`RpcServiceBuilder`].
    #[expect(clippy::type_complexity)]
    pub fn option_layer_rpc_middleware<T>(
        self,
        layer: Option<T>,
    ) -> RpcAddOns<Node, Stack<RpcMiddleware, Either<T, Identity>>, AuthHttpMiddleware> {
        let layer = layer.map(Either::Left).unwrap_or(Either::Right(Identity::new()));
        self.layer_rpc_middleware(layer)
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, Node, BaseNodeEthApi<Node>>,
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
        F: FnOnce(RpcContext<'_, Node, BaseNodeEthApi<Node>>) -> eyre::Result<()> + Send + 'static,
    {
        self.hooks.set_extend_rpc_modules(hook);
        self
    }
}

impl<Node> Default for RpcAddOns<Node, Identity, Identity>
where
    Node: FullNodeComponents,
{
    fn default() -> Self {
        Self::new(BaseEthApiBuilder::default(), Default::default(), Identity::new())
    }
}

impl<N, RpcMiddleware, AuthHttpMiddleware> RpcAddOns<N, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents,
    N::Provider: ChainSpecProvider,
    RpcMiddleware: RethRpcMiddleware,
    AuthHttpMiddleware: RethAuthHttpMiddleware<Identity>,
{
    /// Launches only the regular RPC server (HTTP/WS/IPC), without the authenticated Engine API
    /// server.
    ///
    /// This is useful when you only need the regular RPC functionality and want to avoid
    /// starting the auth server.
    pub async fn launch_rpc_server<F>(
        self,
        ctx: AddOnsContext<'_, N>,
        ext: F,
    ) -> eyre::Result<RpcServerOnlyHandle<N, BaseNodeEthApi<N>>>
    where
        F: FnOnce(RpcModuleContainer<'_, N, BaseNodeEthApi<N>>) -> eyre::Result<()>,
    {
        let rpc_middleware = self.rpc_middleware.clone();
        let tokio_runtime = self.tokio_runtime.clone();
        let setup_ctx = self.setup_rpc_components(ctx, ext).await?;
        let RpcSetupContext {
            node,
            config,
            mut modules,
            mut auth_module,
            auth_config: _,
            mut registry,
            on_rpc_started,
            engine_events,
            engine_handle,
        } = setup_ctx;

        let server_config = config
            .rpc
            .rpc_server_config()
            .set_rpc_middleware(rpc_middleware)
            .with_tokio_runtime(tokio_runtime);
        let rpc_server_handle = Self::launch_rpc_server_internal(server_config, &modules).await?;

        let handles =
            RethRpcServerHandles { rpc: rpc_server_handle.clone(), auth: AuthServerHandle::noop() };
        Self::finalize_rpc_setup(
            &mut registry,
            &mut modules,
            &mut auth_module,
            &node,
            config,
            on_rpc_started,
            handles,
        )?;

        Ok(RpcServerOnlyHandle {
            rpc_server_handle,
            rpc_registry: registry,
            engine_events,
            engine_handle,
        })
    }

    /// Launches the RPC servers with the given context and an additional hook for extending
    /// modules. Whether the auth server is launched depends on the CLI configuration.
    pub async fn launch_add_ons_with<F>(
        self,
        ctx: AddOnsContext<'_, N>,
        ext: F,
    ) -> eyre::Result<RpcHandle<N, BaseNodeEthApi<N>>>
    where
        F: FnOnce(RpcModuleContainer<'_, N, BaseNodeEthApi<N>>) -> eyre::Result<()>,
    {
        // Check CLI config to determine if auth server should be disabled
        let disable_auth = ctx.config.rpc.disable_auth_server;
        self.launch_add_ons_with_opt_engine(ctx, ext, disable_auth).await
    }

    /// Launches the RPC servers with the given context and an additional hook for extending
    /// modules. Optionally disables the auth server based on the `disable_auth` parameter.
    ///
    /// When `disable_auth` is true, the auth server will not be started and a noop handle
    /// will be used instead.
    pub async fn launch_add_ons_with_opt_engine<F>(
        self,
        ctx: AddOnsContext<'_, N>,
        ext: F,
        disable_auth: bool,
    ) -> eyre::Result<RpcHandle<N, BaseNodeEthApi<N>>>
    where
        F: FnOnce(RpcModuleContainer<'_, N, BaseNodeEthApi<N>>) -> eyre::Result<()>,
    {
        let rpc_middleware = self.rpc_middleware.clone();
        let auth_http_middleware = self.auth_http_middleware.clone();
        let tokio_runtime = self.tokio_runtime.clone();
        let setup_ctx = self.setup_rpc_components(ctx, ext).await?;
        let RpcSetupContext {
            node,
            config,
            mut modules,
            mut auth_module,
            auth_config,
            mut registry,
            on_rpc_started,
            engine_events,
            engine_handle,
        } = setup_ctx;

        let server_config = config
            .rpc
            .rpc_server_config()
            .set_rpc_middleware(rpc_middleware)
            .with_tokio_runtime(tokio_runtime);

        let auth_config = auth_config.with_http_middleware(auth_http_middleware);

        let (rpc, auth) = if disable_auth {
            // Only launch the RPC server, use a noop auth handle
            let rpc = Self::launch_rpc_server_internal(server_config, &modules).await?;
            (rpc, AuthServerHandle::noop())
        } else {
            let auth_module_clone = auth_module.clone();
            // launch servers concurrently
            let (rpc, auth) = futures::future::try_join(
                Self::launch_rpc_server_internal(server_config, &modules),
                Self::launch_auth_server_internal(auth_config.start(auth_module_clone)),
            )
            .await?;
            (rpc, auth)
        };

        let handles = RethRpcServerHandles { rpc, auth };

        Self::finalize_rpc_setup(
            &mut registry,
            &mut modules,
            &mut auth_module,
            &node,
            config,
            on_rpc_started,
            handles.clone(),
        )?;

        Ok(RpcHandle {
            rpc_server_handles: handles,
            rpc_registry: registry,
            engine_events,
            beacon_engine_handle: engine_handle,
            engine_shutdown: EngineShutdown::default(),
        })
    }

    /// Common setup for RPC server initialization
    async fn setup_rpc_components<'a, F>(
        self,
        ctx: AddOnsContext<'a, N>,
        ext: F,
    ) -> eyre::Result<RpcSetupContext<'a, N, BaseNodeEthApi<N>>>
    where
        F: FnOnce(RpcModuleContainer<'_, N, BaseNodeEthApi<N>>) -> eyre::Result<()>,
    {
        let Self { eth_api_builder, hooks, .. } = self;

        let engine_api = BaseEngineApiBuilder::build_engine_api(&ctx);
        let AddOnsContext { node, config, beacon_engine_handle, jwt_secret, engine_events } = ctx;

        info!(target: "reth::cli", "Engine API handler initialized");

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

        let auth_config = config.rpc.auth_server_config(jwt_secret)?;
        let module_config = config.rpc.transport_rpc_module_config();
        debug!(target: "reth::cli", http=?module_config.http(), ws=?module_config.ws(), "Using RPC module config");

        let (mut modules, mut auth_module, registry) = RpcModuleBuilder::default()
            .with_provider(node.provider().clone())
            .with_pool(node.pool().clone())
            .with_network(node.network().clone())
            .with_executor(node.task_executor().clone())
            .with_evm_config(node.evm_config().clone())
            .with_consensus(node.consensus().clone())
            .build_with_auth_server(
                module_config,
                engine_api,
                eth_api,
                engine_events.clone(),
                beacon_engine_handle.clone(),
            );

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
            auth_module: &mut auth_module,
        };

        let RpcHooks { on_rpc_started, extend_rpc_modules } = hooks;

        ext(RpcModuleContainer {
            modules: ctx.modules,
            auth_module: ctx.auth_module,
            registry: ctx.registry,
        })?;
        extend_rpc_modules.extend_rpc_modules(ctx)?;

        Ok(RpcSetupContext {
            node,
            config,
            modules,
            auth_module,
            auth_config,
            registry,
            on_rpc_started,
            engine_events,
            engine_handle: beacon_engine_handle,
        })
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

        if let Some(path) = handle.ipc_endpoint() {
            info!(target: "reth::cli", %path, "RPC IPC server started");
        }
        if let Some(addr) = handle.http_local_addr() {
            info!(target: "reth::cli", url=%addr, "RPC HTTP server started");
        }
        if let Some(addr) = handle.ws_local_addr() {
            info!(target: "reth::cli", url=%addr, "RPC WS server started");
        }

        Ok(handle)
    }

    /// Helper to launch the auth server
    async fn launch_auth_server_internal(
        start_fut: impl Future<Output = Result<AuthServerHandle, reth_rpc_builder::error::RpcError>>,
    ) -> eyre::Result<AuthServerHandle> {
        start_fut
            .await
            .map_err(Into::into)
            .inspect(|handle| {
                let addr = handle.local_addr();
                if let Some(ipc_endpoint) = handle.ipc_endpoint() {
                    info!(target: "reth::cli", url=%addr, ipc_endpoint=%ipc_endpoint, "RPC auth server started");
                } else {
                    info!(target: "reth::cli", url=%addr, "RPC auth server started");
                }
            })
    }

    /// Helper to finalize RPC setup by creating context and calling hooks
    fn finalize_rpc_setup(
        registry: &mut RpcRegistry<N, BaseNodeEthApi<N>>,
        modules: &mut TransportRpcModules,
        auth_module: &mut AuthRpcModule,
        node: &N,
        config: &NodeConfig,
        on_rpc_started: Box<dyn OnRpcStarted<N, BaseNodeEthApi<N>>>,
        handles: RethRpcServerHandles,
    ) -> eyre::Result<()> {
        let ctx = RpcContext { node: node.clone(), config, registry, modules, auth_module };

        on_rpc_started.on_rpc_started(ctx, handles)?;
        Ok(())
    }
}

impl<N, RpcMiddleware, AuthHttpMiddleware> NodeAddOns<N>
    for RpcAddOns<N, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents,
    RpcMiddleware: RethRpcMiddleware,
    AuthHttpMiddleware: RethAuthHttpMiddleware<Identity>,
{
    type Handle = RpcHandle<N, BaseNodeEthApi<N>>;

    async fn launch_add_ons(self, ctx: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        self.launch_add_ons_with(ctx, |_| Ok(())).await
    }
}

/// Helper trait implemented for add-ons producing [`RpcHandle`]. Used by common node launcher
/// implementations.
pub trait RethRpcAddOns<N: FullNodeComponents>:
    NodeAddOns<N, Handle = RpcHandle<N, Self::EthApi>>
{
    /// eth API implementation.
    type EthApi: EthApiTypes;

    /// Returns a mutable reference to RPC hooks.
    fn hooks_mut(&mut self) -> &mut RpcHooks<N, Self::EthApi>;
}

impl<N: FullNodeComponents, RpcMiddleware, AuthHttpMiddleware> RethRpcAddOns<N>
    for RpcAddOns<N, RpcMiddleware, AuthHttpMiddleware>
where
    Self: NodeAddOns<N, Handle = RpcHandle<N, BaseNodeEthApi<N>>>,
    BaseNodeEthApi<N>:
        FullEthApiServer<Provider = N::Provider, Pool = reth_node_api::BaseNodePool<N::Provider>>,
{
    type EthApi = BaseNodeEthApi<N>;

    fn hooks_mut(&mut self) -> &mut RpcHooks<N, Self::EthApi> {
        &mut self.hooks
    }
}

/// Constructs the Base execution validator and its caches.
#[derive(Debug, Default, Clone)]
pub struct BasicEngineValidatorBuilder;

impl BasicEngineValidatorBuilder {
    /// Constructs the Base execution validator and its caches.
    pub async fn build_tree_validator<Node: FullNodeComponents>(
        ctx: &AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
        overlay_manager: OverlayManager,
    ) -> eyre::Result<BasicEngineValidator<Node::Provider, BaseEngineValidator>> {
        let validator = BaseEngineValidator::new::<KeccakKeyHasher>(Arc::clone(&ctx.config.chain));
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
            validator = validator
                .with_txpool_prewarming(txpool_prewarm::Source::new(ctx.node.pool().clone()));
        }

        Ok(validator)
    }
}

/// Handle to trigger graceful engine shutdown.
///
/// This handle can be used to request a graceful shutdown of the engine,
/// which will persist all remaining in-memory blocks before terminating.
#[derive(Clone, Debug)]
pub struct EngineShutdown {
    /// Channel to send shutdown signal.
    tx: Arc<Mutex<Option<oneshot::Sender<EngineShutdownRequest>>>>,
}

impl EngineShutdown {
    /// Creates a new [`EngineShutdown`] handle and returns the receiver.
    pub fn new() -> (Self, oneshot::Receiver<EngineShutdownRequest>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx: Arc::new(Mutex::new(Some(tx))) }, rx)
    }

    /// Requests a graceful engine shutdown.
    ///
    /// All remaining in-memory blocks will be persisted before the engine terminates.
    ///
    /// Returns a receiver that resolves when shutdown is complete.
    /// Returns `None` if shutdown was already triggered.
    pub fn shutdown(&self) -> Option<oneshot::Receiver<()>> {
        let mut guard = self.tx.lock();
        let tx = guard.take()?;
        let (done_tx, done_rx) = oneshot::channel();
        let _ = tx.send(EngineShutdownRequest { done_tx });
        Some(done_rx)
    }
}

impl Default for EngineShutdown {
    fn default() -> Self {
        Self { tx: Arc::new(Mutex::new(None)) }
    }
}

/// Request to shutdown the engine.
#[derive(Debug)]
pub struct EngineShutdownRequest {
    /// Channel to signal shutdown completion.
    pub done_tx: oneshot::Sender<()>,
}
