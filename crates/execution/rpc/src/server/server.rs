use std::{
    collections::HashMap,
    fmt::Debug,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base_common_chain_config::ChainSpecProvider;
use base_common_client_ethereum::{
    IntoWallet, Provider, ProviderBuilder, fillers::RecommendedFillers,
};
use base_common_runtime::{EventSender, Runtime, pool::BlockingTaskGuard};
use base_common_types_payload::ConsensusEngineEvent;
use base_common_types_rpc as constants;
use base_execution_evm_blocks::{BaseBeaconConsensus, BaseEvmConfig};
use base_execution_state_provider::providers::BlockchainProvider;
use base_execution_txpool::BaseTransactionPool;
use http::{HeaderMap, header::AUTHORIZATION};
use jsonrpsee::{
    Methods, RpcModule,
    core::RegisterMethodError,
    server::{
        AlreadyStoppedError, ServerBuilder, ServerConfigBuilder, ServerHandle,
        middleware::rpc::RpcServiceBuilder,
    },
};
use serde::{Deserialize, Serialize};
use tower::layer::util::Identity;
use tower_http::cors::CorsLayer;

use crate::{
    AdminApi, AdminApiServer, BaseEthApi, DebugApi, DebugApiServer, EthApiServer, EthBundle,
    EthCallBundleApiServer, EthConfig, EthFilterApiServer, EthPubSubApiServer, EthSimBundle,
    EthSubscriptionIdProvider, MevSimApiServer, MinerApi, MinerApiServer, NetApi, NetApiServer,
    RPCApi, RethApi, RethApiServer, RpcApiServer, TraceApi, TraceApiServer, TxPoolApi,
    TxPoolApiServer, Web3Api, Web3ApiServer,
    server::{
        AuthLayer, Claims, CompressionLayer, CorsDomainError, EthHandlers, JwtAuthValidator,
        JwtSecret, RpcNamespace, cors,
        error::{RpcError, ServerKind, WsHttpSamePortError},
        metrics::RpcRequestMetrics,
        middleware::RethRpcMiddleware,
    },
};

/// Bundles settings for modules
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RpcModuleConfig {
    /// `eth` namespace settings
    eth: EthConfig,
}

// === impl RpcModuleConfig ===

impl RpcModuleConfig {
    /// Returns a new RPC module config given the eth namespace config
    pub const fn new(eth: EthConfig) -> Self {
        Self { eth }
    }

    /// Get a reference to the eth namespace config
    pub const fn eth(&self) -> &EthConfig {
        &self.eth
    }

    /// Get a mutable reference to the eth namespace config
    pub const fn eth_mut(&mut self) -> &mut EthConfig {
        &mut self.eth
    }
}

/// A Helper type the holds instances of the configured modules.
#[derive(Debug)]
pub struct RpcRegistryInner {
    provider: BlockchainProvider,
    pool: BaseTransactionPool,
    network: base_execution_network_service::NetworkHandle,
    executor: Runtime,
    evm_config: BaseEvmConfig,
    consensus: Arc<BaseBeaconConsensus>,
    /// Holds all `eth_` namespace handlers
    eth: EthHandlers,
    /// to put trace calls behind semaphore
    blocking_pool_guard: BlockingTaskGuard,
    /// Contains the [Methods] of a module
    modules: HashMap<RpcNamespace, Methods>,
    /// eth config settings
    eth_config: EthConfig,
    /// Notification channel for engine API events
    engine_events: EventSender<ConsensusEngineEvent>,
}

// === impl RpcRegistryInner ===

impl RpcRegistryInner {
    /// Creates a new, empty instance.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        provider: BlockchainProvider,
        pool: BaseTransactionPool,
        network: base_execution_network_service::NetworkHandle,
        executor: Runtime,
        consensus: Arc<BaseBeaconConsensus>,
        config: RpcModuleConfig,
        evm_config: BaseEvmConfig,
        eth_api: BaseEthApi,
        engine_events: EventSender<ConsensusEngineEvent>,
    ) -> Self {
        let blocking_pool_guard = BlockingTaskGuard::new(config.eth.max_tracing_requests);

        let eth = EthHandlers::bootstrap(config.eth.clone(), executor.clone(), eth_api);

        Self {
            provider,
            pool,
            network,
            eth,
            executor,
            consensus,
            modules: Default::default(),
            blocking_pool_guard,
            eth_config: config.eth,
            evm_config,
            engine_events,
        }
    }
}

impl RpcRegistryInner {
    /// Returns a reference to the installed [`EthApi`].
    pub const fn eth_api(&self) -> &BaseEthApi {
        &self.eth.api
    }

    /// Returns a reference to the installed [`EthHandlers`].
    pub const fn eth_handlers(&self) -> &EthHandlers {
        &self.eth
    }

    /// Returns a reference to the pool
    pub const fn pool(&self) -> &BaseTransactionPool {
        &self.pool
    }

    /// Returns a reference to the tasks type
    pub const fn tasks(&self) -> &Runtime {
        &self.executor
    }

    /// Returns a reference to the provider
    pub const fn provider(&self) -> &BlockchainProvider {
        &self.provider
    }

    /// Returns a reference to the evm config
    pub const fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    /// Returns all installed methods
    pub fn methods(&self) -> Vec<Methods> {
        self.modules.values().cloned().collect()
    }

    /// Returns a merged `RpcModule`
    pub fn module(&self) -> RpcModule<()> {
        let mut module = RpcModule::new(());
        for methods in self.modules.values().cloned() {
            module.merge(methods).expect("No conflicts");
        }
        module
    }
}

impl RpcRegistryInner {
    /// Instantiates `AdminApi`
    pub fn admin_api(&self) -> AdminApi {
        AdminApi::new(self.network.clone(), self.provider.chain_spec(), self.pool.clone())
    }

    /// Instantiates `Web3Api`
    pub fn web3_api(&self) -> Web3Api {
        Web3Api::new(self.network.clone())
    }
}

impl RpcRegistryInner {
    /// Instantiates `TraceApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn trace_api(&self) -> TraceApi {
        TraceApi::new(
            self.eth_api().clone(),
            self.blocking_pool_guard.clone(),
            self.eth_config.clone(),
        )
    }

    /// Instantiates [`EthBundle`] Api
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn bundle_api(&self) -> EthBundle {
        let eth_api = self.eth_api().clone();
        EthBundle::new(eth_api, self.blocking_pool_guard.clone())
    }

    /// Instantiates `DebugApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn debug_api(&self) -> DebugApi {
        DebugApi::new(
            self.eth_api().clone(),
            self.blocking_pool_guard.clone(),
            self.tasks(),
            self.engine_events.new_listener(),
        )
    }

    /// Instantiates `NetApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn net_api(&self) -> NetApi {
        let eth_api = self.eth_api().clone();
        NetApi::new(self.network.clone(), eth_api)
    }

    /// Instantiates `RethApi`
    pub fn reth_api(&self) -> RethApi {
        RethApi::new(
            self.provider.clone(),
            self.evm_config.clone(),
            self.blocking_pool_guard.clone(),
            self.executor.clone(),
        )
    }
}

impl RpcRegistryInner {
    /// Configure a [`TransportRpcModules`] using the current registry. This
    /// creates [`RpcModule`] instances for the modules selected by the
    /// `config`.
    pub fn create_transport_rpc_modules(
        &mut self,
        config: TransportRpcModuleConfig,
    ) -> TransportRpcModules<()> {
        let mut modules = TransportRpcModules::default();
        let http = config.http.then(|| self.base_module());
        let ws = config.ws.then(|| self.base_module());

        modules.config = config;
        modules.http = http;
        modules.ws = ws;

        modules
    }

    /// Builds the fixed Base API set.
    pub fn base_module(&mut self) -> RpcModule<()> {
        let mut module = RpcModule::new(());
        let all_methods = self.reth_methods();
        for methods in all_methods {
            module.merge(methods).expect("No conflicts");
        }
        module
    }

    /// Returns the [Methods] for the given [`RpcNamespace`]
    ///
    /// If this is the first time the namespace is requested, a new instance of API implementation
    /// will be created.
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn reth_methods(&mut self) -> Vec<Methods> {
        let EthHandlers { api: eth_api, filter: eth_filter, pubsub: eth_pubsub, .. } =
            self.eth_handlers().clone();

        // Create a copy, so we can list out all the methods for rpc_ api
        let namespaces: Vec<_> = RpcNamespace::modules().collect();
        namespaces
            .iter()
            .map(|namespace| {
                self.modules
                    .entry(namespace.clone())
                    .or_insert_with(|| match namespace.clone() {
                        RpcNamespace::Admin => AdminApi::new(
                            self.network.clone(),
                            self.provider.chain_spec(),
                            self.pool.clone(),
                        )
                        .into_rpc()
                        .into(),
                        RpcNamespace::Debug => DebugApi::new(
                            eth_api.clone(),
                            self.blocking_pool_guard.clone(),
                            &self.executor,
                            self.engine_events.new_listener(),
                        )
                        .into_rpc()
                        .into(),
                        RpcNamespace::Eth => {
                            // merge all eth handlers
                            let mut module = eth_api.clone().into_rpc();
                            module.merge(eth_filter.clone().into_rpc()).expect("No conflicts");
                            module.merge(eth_pubsub.clone().into_rpc()).expect("No conflicts");
                            module
                                .merge(
                                    EthBundle::new(
                                        eth_api.clone(),
                                        self.blocking_pool_guard.clone(),
                                    )
                                    .into_rpc(),
                                )
                                .expect("No conflicts");

                            module.into()
                        }
                        RpcNamespace::Net => {
                            NetApi::new(self.network.clone(), eth_api.clone()).into_rpc().into()
                        }
                        RpcNamespace::Trace => TraceApi::new(
                            eth_api.clone(),
                            self.blocking_pool_guard.clone(),
                            self.eth_config.clone(),
                        )
                        .into_rpc()
                        .into(),
                        RpcNamespace::Web3 => Web3Api::new(self.network.clone()).into_rpc().into(),
                        RpcNamespace::Txpool => TxPoolApi::new(
                            self.eth.api.pool().clone(),
                            dyn_clone::clone(self.eth.api.converter()),
                        )
                        .into_rpc()
                        .into(),
                        RpcNamespace::Rpc => RPCApi::new(
                            namespaces
                                .iter()
                                .map(|module| (module.to_string(), "1.0".to_string()))
                                .collect(),
                        )
                        .into_rpc()
                        .into(),
                        RpcNamespace::Reth => RethApi::new(
                            self.provider.clone(),
                            self.evm_config.clone(),
                            self.blocking_pool_guard.clone(),
                            self.executor.clone(),
                        )
                        .into_rpc()
                        .into(),
                        RpcNamespace::Miner => MinerApi::default().into_rpc().into(),
                        RpcNamespace::Mev => {
                            EthSimBundle::new(eth_api.clone(), self.blocking_pool_guard.clone())
                                .into_rpc()
                                .into()
                        }
                    })
                    .clone()
            })
            .collect::<Vec<_>>()
    }
}

impl Clone for RpcRegistryInner {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            pool: self.pool.clone(),
            network: self.network.clone(),
            executor: self.executor.clone(),
            evm_config: self.evm_config.clone(),
            consensus: self.consensus.clone(),
            eth: self.eth.clone(),
            blocking_pool_guard: self.blocking_pool_guard.clone(),
            modules: self.modules.clone(),
            eth_config: self.eth_config.clone(),
            engine_events: self.engine_events.clone(),
        }
    }
}

/// A builder type for configuring and launching the servers that will handle RPC requests.
///
/// Supported server transports are:
///    - http
///    - ws
///
/// Http and WS share the same settings: [`ServerBuilder`].
///
/// Once the [`RpcModule`] is assembled by [`RpcRegistryInner`] the servers can be started, See also
/// [`ServerBuilder::build`] and [`Server::start`](jsonrpsee::server::Server::start).
#[derive(Debug)]
pub struct RpcServerConfig<RpcMiddleware = Identity> {
    /// Configs for JSON-RPC Http.
    http_server_config: Option<ServerConfigBuilder>,
    /// Allowed CORS Domains for http
    http_cors_domains: Option<String>,
    /// Address where to bind the http server to
    http_addr: Option<SocketAddr>,
    /// Control whether http responses should be compressed
    http_disable_compression: bool,
    /// Configs for WS server
    ws_server_config: Option<ServerConfigBuilder>,
    /// Allowed CORS Domains for ws.
    ws_cors_domains: Option<String>,
    /// Address where to bind the ws server to
    ws_addr: Option<SocketAddr>,

    /// JWT secret for authentication
    jwt_secret: Option<JwtSecret>,
    /// Whether RPC request metrics are enabled.
    rpc_metrics_enabled: bool,
    /// Configurable RPC middleware
    rpc_middleware: RpcMiddleware,
}

// === impl RpcServerConfig ===

impl Default for RpcServerConfig<Identity> {
    /// Create a new config instance
    fn default() -> Self {
        Self {
            http_server_config: None,
            http_cors_domains: None,
            http_addr: None,
            http_disable_compression: false,
            ws_server_config: None,
            ws_cors_domains: None,
            ws_addr: None,

            jwt_secret: None,
            rpc_metrics_enabled: true,
            rpc_middleware: Default::default(),
        }
    }
}

impl RpcServerConfig {
    /// Creates a new config with only http set
    pub fn http(config: ServerConfigBuilder) -> Self {
        Self::default().with_http(config)
    }

    /// Creates a new config with only ws set
    pub fn ws(config: ServerConfigBuilder) -> Self {
        Self::default().with_ws(config)
    }

    /// Configures the http server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// compatibility with Ethereum subscription clients.
    pub fn with_http(mut self, config: ServerConfigBuilder) -> Self {
        self.http_server_config =
            Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }

    /// Configures the ws server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// compatibility with Ethereum subscription clients.
    pub fn with_ws(mut self, config: ServerConfigBuilder) -> Self {
        self.ws_server_config = Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }
}

impl<RpcMiddleware> RpcServerConfig<RpcMiddleware> {
    /// Configure rpc middleware
    pub fn set_rpc_middleware<T>(self, rpc_middleware: T) -> RpcServerConfig<T> {
        RpcServerConfig {
            http_server_config: self.http_server_config,
            http_cors_domains: self.http_cors_domains,
            http_addr: self.http_addr,
            http_disable_compression: self.http_disable_compression,
            ws_server_config: self.ws_server_config,
            ws_cors_domains: self.ws_cors_domains,
            ws_addr: self.ws_addr,

            jwt_secret: self.jwt_secret,
            rpc_metrics_enabled: self.rpc_metrics_enabled,
            rpc_middleware,
        }
    }

    /// Configures whether the built-in RPC request metrics layer is enabled.
    pub const fn with_rpc_metrics_enabled(mut self, enabled: bool) -> Self {
        self.rpc_metrics_enabled = enabled;
        self
    }

    /// Configure the cors domains for http _and_ ws
    pub fn with_cors(self, cors_domain: Option<String>) -> Self {
        self.with_http_cors(cors_domain.clone()).with_ws_cors(cors_domain)
    }

    /// Configure the cors domains for WS
    pub fn with_ws_cors(mut self, cors_domain: Option<String>) -> Self {
        self.ws_cors_domains = cors_domain;
        self
    }

    /// Configure whether HTTP responses should be compressed
    pub const fn with_http_disable_compression(mut self, http_disable_compression: bool) -> Self {
        self.http_disable_compression = http_disable_compression;
        self
    }

    /// Configure the cors domains for HTTP
    pub fn with_http_cors(mut self, cors_domain: Option<String>) -> Self {
        self.http_cors_domains = cors_domain;
        self
    }

    /// Configures the [`SocketAddr`] of the http server
    ///
    /// Default is [`Ipv4Addr::LOCALHOST`] and
    /// [`base_common_types_rpc::DEFAULT_HTTP_RPC_PORT`]
    pub const fn with_http_address(mut self, addr: SocketAddr) -> Self {
        self.http_addr = Some(addr);
        self
    }

    /// Configures the [`SocketAddr`] of the ws server
    ///
    /// Default is [`Ipv4Addr::LOCALHOST`] and
    /// [`base_common_types_rpc::DEFAULT_WS_RPC_PORT`]
    pub const fn with_ws_address(mut self, addr: SocketAddr) -> Self {
        self.ws_addr = Some(addr);
        self
    }

    /// Configures the JWT secret for authentication.
    pub const fn with_jwt_secret(mut self, secret: Option<JwtSecret>) -> Self {
        self.jwt_secret = secret;
        self
    }

    /// Returns true if any server is configured.
    ///
    /// If no server is configured, no server will be launched on [`RpcServerConfig::start`].
    pub const fn has_server(&self) -> bool {
        self.http_server_config.is_some() || self.ws_server_config.is_some()
    }

    /// Returns the [`SocketAddr`] of the http server
    pub const fn http_address(&self) -> Option<SocketAddr> {
        self.http_addr
    }

    /// Returns the [`SocketAddr`] of the ws server
    pub const fn ws_address(&self) -> Option<SocketAddr> {
        self.ws_addr
    }

    /// Returns whether the built-in RPC request metrics layer is enabled.
    pub const fn rpc_metrics_enabled(&self) -> bool {
        self.rpc_metrics_enabled
    }

    /// Creates the [`CorsLayer`] if any
    fn maybe_cors_layer(cors: Option<String>) -> Result<Option<CorsLayer>, CorsDomainError> {
        cors.as_deref().map(cors::create_cors_layer).transpose()
    }

    /// Creates the [`AuthLayer`] if any
    fn maybe_jwt_layer(jwt_secret: Option<JwtSecret>) -> Option<AuthLayer> {
        jwt_secret.map(|secret| AuthLayer::new(JwtAuthValidator::new(secret)))
    }

    /// Returns a [`CompressionLayer`] that adds compression support (gzip, deflate, brotli, zstd)
    /// based on the client's `Accept-Encoding` header
    fn maybe_compression_layer(disable_compression: bool) -> Option<CompressionLayer> {
        if disable_compression { None } else { Some(CompressionLayer::new()) }
    }

    ///
    /// If both http and ws are on the same port, they are combined into one server.
    ///
    /// Returns the [`RpcServerHandle`] with the handle to the started servers.
    pub async fn start(self, modules: &TransportRpcModules) -> Result<RpcServerHandle, RpcError>
    where
        RpcMiddleware: RethRpcMiddleware,
    {
        let mut http_handle = None;
        let mut ws_handle = None;

        let http_socket_addr = self.http_addr.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            constants::DEFAULT_HTTP_RPC_PORT,
        )));

        let ws_socket_addr = self.ws_addr.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            constants::DEFAULT_WS_RPC_PORT,
        )));

        let rpc_metrics_enabled = self.rpc_metrics_enabled;

        // If both are configured on the same port, we combine them into one server.
        if self.http_addr == self.ws_addr
            && self.http_server_config.is_some()
            && self.ws_server_config.is_some()
        {
            let cors = match (self.ws_cors_domains.as_ref(), self.http_cors_domains.as_ref()) {
                (Some(ws_cors), Some(http_cors)) => {
                    if ws_cors.trim() != http_cors.trim() {
                        return Err(WsHttpSamePortError::ConflictingCorsDomains {
                            http_cors_domains: Some(http_cors.clone()),
                            ws_cors_domains: Some(ws_cors.clone()),
                        }
                        .into());
                    }
                    Some(ws_cors)
                }
                (a, b) => a.or(b),
            }
            .cloned();

            // we merge this into one server using the http setup

            if let Some(config) = self.http_server_config {
                let server = ServerBuilder::new()
                    .set_http_middleware(
                        tower::ServiceBuilder::new()
                            .option_layer(Self::maybe_cors_layer(cors)?)
                            .option_layer(Self::maybe_jwt_layer(self.jwt_secret))
                            .option_layer(Self::maybe_compression_layer(
                                self.http_disable_compression,
                            )),
                    )
                    .set_rpc_middleware(
                        RpcServiceBuilder::default()
                            .option_layer(
                                rpc_metrics_enabled
                                    .then(|| {
                                        modules
                                            .http
                                            .as_ref()
                                            .or(modules.ws.as_ref())
                                            .map(RpcRequestMetrics::same_port)
                                    })
                                    .flatten(),
                            )
                            .layer(self.rpc_middleware.clone()),
                    )
                    .set_config(config.build())
                    .build(http_socket_addr)
                    .await
                    .map_err(|err| {
                        RpcError::server_error(err, ServerKind::WsHttp(http_socket_addr))
                    })?;
                let addr = server.local_addr().map_err(|err| {
                    RpcError::server_error(err, ServerKind::WsHttp(http_socket_addr))
                })?;
                if let Some(module) = modules.http.as_ref().or(modules.ws.as_ref()) {
                    let handle = server.start(module.clone());
                    http_handle = Some(handle.clone());
                    ws_handle = Some(handle);
                }
                return Ok(RpcServerHandle {
                    http_local_addr: Some(addr),
                    ws_local_addr: Some(addr),
                    http: http_handle,
                    ws: ws_handle,

                    jwt_secret: self.jwt_secret,
                });
            }
        }

        let mut ws_local_addr = None;
        let mut ws_server = None;
        let mut http_local_addr = None;
        let mut http_server = None;

        if let Some(config) = self.ws_server_config {
            let server = ServerBuilder::new()
                .set_config(config.ws_only().build())
                .set_http_middleware(
                    tower::ServiceBuilder::new()
                        .option_layer(Self::maybe_cors_layer(self.ws_cors_domains.clone())?)
                        .option_layer(Self::maybe_jwt_layer(self.jwt_secret)),
                )
                .set_rpc_middleware(
                    RpcServiceBuilder::default()
                        .option_layer(
                            rpc_metrics_enabled
                                .then(|| modules.ws.as_ref().map(RpcRequestMetrics::ws))
                                .flatten(),
                        )
                        .layer(self.rpc_middleware.clone()),
                )
                .build(ws_socket_addr)
                .await
                .map_err(|err| RpcError::server_error(err, ServerKind::WS(ws_socket_addr)))?;

            let addr = server
                .local_addr()
                .map_err(|err| RpcError::server_error(err, ServerKind::WS(ws_socket_addr)))?;

            ws_local_addr = Some(addr);
            ws_server = Some(server);
        }

        if let Some(config) = self.http_server_config {
            let server = ServerBuilder::new()
                .set_config(config.http_only().build())
                .set_http_middleware(
                    tower::ServiceBuilder::new()
                        .option_layer(Self::maybe_cors_layer(self.http_cors_domains.clone())?)
                        .option_layer(Self::maybe_jwt_layer(self.jwt_secret))
                        .option_layer(Self::maybe_compression_layer(self.http_disable_compression)),
                )
                .set_rpc_middleware(
                    RpcServiceBuilder::default()
                        .option_layer(
                            rpc_metrics_enabled
                                .then(|| modules.http.as_ref().map(RpcRequestMetrics::http))
                                .flatten(),
                        )
                        .layer(self.rpc_middleware.clone()),
                )
                .build(http_socket_addr)
                .await
                .map_err(|err| RpcError::server_error(err, ServerKind::Http(http_socket_addr)))?;
            let local_addr = server
                .local_addr()
                .map_err(|err| RpcError::server_error(err, ServerKind::Http(http_socket_addr)))?;
            http_local_addr = Some(local_addr);
            http_server = Some(server);
        }

        http_handle = http_server
            .map(|http_server| http_server.start(modules.http.clone().expect("http server error")));
        ws_handle = ws_server
            .map(|ws_server| ws_server.start(modules.ws.clone().expect("ws server error")));
        Ok(RpcServerHandle {
            http_local_addr,
            ws_local_addr,
            http: http_handle,
            ws: ws_handle,

            jwt_secret: self.jwt_secret,
        })
    }
}

/// Holds modules to be installed per transport type
///
/// # Example
///
/// Configure a http transport only
///
/// ```
/// use base_execution_rpc::TransportRpcModuleConfig;
/// let config =
///     TransportRpcModuleConfig::default().with_http();
/// ```
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct TransportRpcModuleConfig {
    /// http module configuration
    http: bool,
    /// ws module configuration
    ws: bool,

    /// Config for the modules
    config: Option<RpcModuleConfig>,
}

// === impl TransportRpcModuleConfig ===

impl TransportRpcModuleConfig {
    /// Enables HTTP with the built-in Base APIs.
    pub fn set_http() -> Self {
        Self::default().with_http()
    }
    /// Enables WebSocket with the built-in Base APIs.
    pub fn set_ws() -> Self {
        Self::default().with_ws()
    }
    /// Enables HTTP with the built-in Base APIs.
    pub const fn with_http(mut self) -> Self {
        self.http = true;
        self
    }
    /// Enables WebSocket with the built-in Base APIs.
    pub const fn with_ws(mut self) -> Self {
        self.ws = true;
        self
    }
    /// Sets handler limits and caches.
    pub fn with_config(mut self, config: RpcModuleConfig) -> Self {
        self.config = Some(config);
        self
    }
    /// Returns mutable handler settings.
    pub const fn config_mut(&mut self) -> &mut Option<RpcModuleConfig> {
        &mut self.config
    }
    /// Whether both transports are disabled.
    pub const fn is_empty(&self) -> bool {
        !self.http && !self.ws
    }
    /// Whether HTTP is enabled.
    pub const fn http(&self) -> bool {
        self.http
    }
    /// Whether WebSocket is enabled.
    pub const fn ws(&self) -> bool {
        self.ws
    }
    /// Returns handler settings.
    pub const fn config(&self) -> Option<&RpcModuleConfig> {
        self.config.as_ref()
    }
}

/// Holds installed modules per transport type.
#[derive(Debug, Clone, Default)]
pub struct TransportRpcModules<Context = ()> {
    /// The original config
    config: TransportRpcModuleConfig,
    /// rpcs module for http
    http: Option<RpcModule<Context>>,
    /// rpcs module for ws
    ws: Option<RpcModule<Context>>,
}

// === impl TransportRpcModules ===

impl TransportRpcModules {
    /// Sets a custom [`TransportRpcModuleConfig`] for the configured modules.
    /// This will overwrite current configuration, if any.
    pub fn with_config(mut self, config: TransportRpcModuleConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets the [`RpcModule`] for the http transport.
    /// This will overwrite current module, if any.
    pub fn with_http(mut self, http: RpcModule<()>) -> Self {
        self.http = Some(http);
        self
    }

    /// Sets the [`RpcModule`] for the ws transport.
    /// This will overwrite current module, if any.
    pub fn with_ws(mut self, ws: RpcModule<()>) -> Self {
        self.ws = Some(ws);
        self
    }

    /// Returns the [`TransportRpcModuleConfig`] used to configure this instance.
    pub const fn module_config(&self) -> &TransportRpcModuleConfig {
        &self.config
    }

    /// Merge the given [Methods] in the configured http methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no http transport is configured.
    pub fn merge_http(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut http) = self.http {
            return http.merge(other.into()).map(|_| true);
        }
        Ok(false)
    }

    /// Merge the given [Methods] in the configured ws methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no ws transport is configured.
    pub fn merge_ws(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut ws) = self.ws {
            return ws.merge(other.into()).map(|_| true);
        }
        Ok(false)
    }

    /// Merge the given [`Methods`] in all configured methods.
    ///
    /// Fails if any of the methods in other is present already.
    pub fn merge_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        self.merge_http(other.clone())?;
        self.merge_ws(other.clone())?;

        Ok(())
    }

    /// Returns all unique endpoints installed for the given module.
    ///
    /// Note: In case of duplicate method names this only record the first occurrence.
    pub fn methods_by_module(&self, module: RpcNamespace) -> Methods {
        self.methods_by(|name| name.starts_with(module.as_str()))
    }

    /// Returns all unique endpoints installed in any of the configured modules.
    ///
    /// Note: In case of duplicate method names this only record the first occurrence.
    pub fn methods_by<F>(&self, mut filter: F) -> Methods
    where
        F: FnMut(&str) -> bool,
    {
        let mut methods = Methods::new();

        // filter that matches the given filter and also removes duplicates we already have
        let mut f =
            |name: &str, mm: &Methods| filter(name) && !mm.method_names().any(|m| m == name);

        if let Some(m) = self.http_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }
        if let Some(m) = self.ws_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }

        methods
    }

    /// Returns all [`Methods`] installed for the http server based in the given closure.
    ///
    /// Returns `None` if no http support is configured.
    pub fn http_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.http.as_ref().map(|module| methods_by(module, filter))
    }

    /// Returns all [`Methods`] installed for the ws server based in the given closure.
    ///
    /// Returns `None` if no ws support is configured.
    pub fn ws_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.ws.as_ref().map(|module| methods_by(module, filter))
    }

    /// Removes the method with the given name from the configured http methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_http_method(&mut self, method_name: &'static str) -> bool {
        if let Some(http_module) = &mut self.http {
            http_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured http methods.
    pub fn remove_http_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_http_method(name);
        }
    }

    /// Removes the method with the given name from the configured ws methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_ws_method(&mut self, method_name: &'static str) -> bool {
        if let Some(ws_module) = &mut self.ws {
            ws_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured ws methods.
    pub fn remove_ws_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_ws_method(name);
        }
    }

    /// Removes the method with the given name from all configured transports.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    pub fn remove_method_from_configured(&mut self, method_name: &'static str) -> bool {
        let http_removed = self.remove_http_method(method_name);
        let ws_removed = self.remove_ws_method(method_name);

        http_removed || ws_removed
    }

    /// Renames a method in all configured transports by:
    /// 1. Removing the old method name.
    /// 2. Adding the new method.
    pub fn rename(
        &mut self,
        old_name: &'static str,
        new_method: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        // Remove the old method from all configured transports
        self.remove_method_from_configured(old_name);

        // Merge the new method into the configured transports
        self.merge_configured(new_method)
    }

    /// Replace the given [`Methods`] in the configured http methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no http transport is configured.
    pub fn replace_http(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_http_methods(other.method_names());
        self.merge_http(other)
    }

    /// Replace the given [Methods] in the configured ws methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no ws transport is configured.
    pub fn replace_ws(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ws_methods(other.method_names());
        self.merge_ws(other)
    }

    /// Replaces the method with the given name from all configured transports.
    ///
    /// Returns `true` if the method was found and replaced, `false` otherwise
    pub fn replace_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.replace_http(other.clone())?;
        self.replace_ws(other.clone())?;
        Ok(true)
    }

    /// Adds or replaces given [`Methods`] in http module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_http(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_http_methods(other.method_names());
        self.merge_http(other)
    }

    /// Adds or replaces given [`Methods`] in ws module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_ws(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ws_methods(other.method_names());
        self.merge_ws(other)
    }

    /// Adds or replaces given [`Methods`] in all configured network modules.
    pub fn add_or_replace_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        self.add_or_replace_http(other.clone())?;
        self.add_or_replace_ws(other.clone())?;

        Ok(())
    }
}

/// Returns the methods installed in the given module that match the given filter.
fn methods_by<T, F>(module: &RpcModule<T>, mut filter: F) -> Methods
where
    F: FnMut(&str) -> bool,
{
    let mut methods = Methods::new();
    let method_names = module.method_names().filter(|name| filter(name));

    for name in method_names {
        if let Some(matched_method) = module.method(name).cloned() {
            let _ = methods.verify_and_insert(name, matched_method);
        }
    }

    methods
}

/// A handle to the spawned servers.
///
/// When this type is dropped or [`RpcServerHandle::stop`] has been called the server will be
/// stopped.
#[derive(Clone, Debug)]
#[must_use = "Server stops if dropped"]
pub struct RpcServerHandle {
    /// The address of the http/ws server
    http_local_addr: Option<SocketAddr>,
    ws_local_addr: Option<SocketAddr>,
    http: Option<ServerHandle>,
    ws: Option<ServerHandle>,

    jwt_secret: Option<JwtSecret>,
}

// === impl RpcServerHandle ===

impl RpcServerHandle {
    /// Configures the JWT secret for authentication.
    fn bearer_token(&self) -> Option<String> {
        self.jwt_secret.as_ref().map(|secret| {
            format!(
                "Bearer {}",
                secret
                    .encode(&Claims {
                        iat: (SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
                            + Duration::from_secs(60))
                        .as_secs(),
                        exp: None,
                    })
                    .unwrap()
            )
        })
    }
    /// Returns the [`SocketAddr`] of the http server if started.
    pub const fn http_local_addr(&self) -> Option<SocketAddr> {
        self.http_local_addr
    }

    /// Returns the [`SocketAddr`] of the ws server if started.
    pub const fn ws_local_addr(&self) -> Option<SocketAddr> {
        self.ws_local_addr
    }

    /// Tell the server to stop without waiting for the server to stop.
    pub fn stop(self) -> Result<(), AlreadyStoppedError> {
        if let Some(handle) = self.http {
            handle.stop()?
        }

        if let Some(handle) = self.ws {
            handle.stop()?
        }

        Ok(())
    }

    /// Returns the url to the http server
    pub fn http_url(&self) -> Option<String> {
        self.http_local_addr.map(|addr| format!("http://{addr}"))
    }

    /// Returns the url to the ws server
    pub fn ws_url(&self) -> Option<String> {
        self.ws_local_addr.map(|addr| format!("ws://{addr}"))
    }

    /// Returns a http client connected to the server.
    pub fn http_client(&self) -> Option<jsonrpsee::http_client::HttpClient> {
        let url = self.http_url()?;

        let client = if let Some(token) = self.bearer_token() {
            jsonrpsee::http_client::HttpClientBuilder::default()
                .set_headers(HeaderMap::from_iter([(AUTHORIZATION, token.parse().unwrap())]))
                .build(url)
        } else {
            jsonrpsee::http_client::HttpClientBuilder::default().build(url)
        };

        client.expect("failed to create http client").into()
    }

    /// Returns a ws client connected to the server.
    pub async fn ws_client(&self) -> Option<jsonrpsee::ws_client::WsClient> {
        let url = self.ws_url()?;
        let mut builder = jsonrpsee::ws_client::WsClientBuilder::default();

        if let Some(token) = self.bearer_token() {
            let headers = HeaderMap::from_iter([(AUTHORIZATION, token.parse().unwrap())]);
            builder = builder.set_headers(headers);
        }

        let client = builder.build(url).await.expect("failed to create ws client");
        Some(client)
    }

    /// Returns a new [`base_common_client_ethereum::Ethereum`] http provider with its recommended fillers.
    pub fn eth_http_provider(
        &self,
    ) -> Option<impl Provider<base_common_client_ethereum::Ethereum> + Clone + Unpin + 'static>
    {
        self.new_http_provider_for()
    }

    /// Returns a new [`base_common_client_ethereum::Ethereum`] http provider with its recommended fillers and
    /// installed wallet.
    pub fn eth_http_provider_with_wallet<W>(
        &self,
        wallet: W,
    ) -> Option<impl Provider<base_common_client_ethereum::Ethereum> + Clone + Unpin + 'static>
    where
        W: IntoWallet<
                base_common_client_ethereum::Ethereum,
                NetworkWallet: Clone + Unpin + 'static,
            >,
    {
        let rpc_url = self.http_url()?;
        let provider =
            ProviderBuilder::new().wallet(wallet).connect_http(rpc_url.parse().expect("valid url"));
        Some(provider)
    }

    /// Returns an http provider from the rpc server handle for the
    /// specified [`base_common_client_ethereum::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub fn new_http_provider_for<N>(&self) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.http_url()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect_http(rpc_url.parse().expect("valid url"));
        Some(provider)
    }

    /// Returns a new [`base_common_client_ethereum::Ethereum`] websocket provider with its recommended fillers.
    pub async fn eth_ws_provider(
        &self,
    ) -> Option<impl Provider<base_common_client_ethereum::Ethereum> + Clone + Unpin + 'static>
    {
        self.new_ws_provider_for().await
    }

    /// Returns a new [`base_common_client_ethereum::Ethereum`] ws provider with its recommended fillers and
    /// installed wallet.
    pub async fn eth_ws_provider_with_wallet<W>(
        &self,
        wallet: W,
    ) -> Option<impl Provider<base_common_client_ethereum::Ethereum> + Clone + Unpin + 'static>
    where
        W: IntoWallet<
                base_common_client_ethereum::Ethereum,
                NetworkWallet: Clone + Unpin + 'static,
            >,
    {
        let rpc_url = self.ws_url()?;
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect(&rpc_url)
            .await
            .expect("failed to create ws client");
        Some(provider)
    }

    /// Returns an ws provider from the rpc server handle for the
    /// specified [`base_common_client_ethereum::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub async fn new_ws_provider_for<N>(&self) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.ws_url()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect(&rpc_url)
            .await
            .expect("failed to create ws client");
        Some(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rpc_module_str() {
        macro_rules! assert_rpc_module {
            ($($s:expr => $v:expr,)*) => {
                $(
                    let val: RpcNamespace  = $s.parse().unwrap();
                    assert_eq!(val, $v);
                    assert_eq!(val.to_string(), $s);
                )*
            };
        }
        assert_rpc_module!
        (
                "admin" =>  RpcNamespace::Admin,
                "debug" =>  RpcNamespace::Debug,
                "eth" =>  RpcNamespace::Eth,
                "net" =>  RpcNamespace::Net,
                "trace" =>  RpcNamespace::Trace,
                "web3" =>  RpcNamespace::Web3,
                "rpc" => RpcNamespace::Rpc,
                "reth" => RpcNamespace::Reth,
            );
    }

    fn create_test_module() -> RpcModule<()> {
        let mut module = RpcModule::new(());
        module.register_method("anything", |_, _, _| "succeed").unwrap();
        module
    }

    #[test]
    fn test_remove_http_method() {
        let mut modules =
            TransportRpcModules { http: Some(create_test_module()), ..Default::default() };
        // Remove a method that exists
        assert!(modules.remove_http_method("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_http_method("non_existent_method"));

        // Verify that the method was removed
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_remove_ws_method() {
        let mut modules =
            TransportRpcModules { ws: Some(create_test_module()), ..Default::default() };

        // Remove a method that exists
        assert!(modules.remove_ws_method("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_ws_method("non_existent_method"));

        // Verify that the method was removed
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_remove_method_from_configured() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),

            ..Default::default()
        };

        // Remove a method that exists
        assert!(modules.remove_method_from_configured("anything"));

        // Remove a method that was just removed (it does not exist anymore)
        assert!(!modules.remove_method_from_configured("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_method_from_configured("non_existent_method"));

        // Verify that the method was removed from all transports
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_transport_rpc_module_rename() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),

            ..Default::default()
        };

        // Verify that the old we want to rename exists at the start
        assert!(modules.http.as_ref().unwrap().method("anything").is_some());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());

        // Verify that the new method does not exist at the start
        assert!(modules.http.as_ref().unwrap().method("something").is_none());
        assert!(modules.ws.as_ref().unwrap().method("something").is_none());

        // Create another module
        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        // Rename the method
        modules.rename("anything", other_module).expect("rename failed");

        // Verify that the old method was removed from all transports
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());

        // Verify that the new method was added to all transports
        assert!(modules.http.as_ref().unwrap().method("something").is_some());
        assert!(modules.ws.as_ref().unwrap().method("something").is_some());
    }

    #[test]
    fn test_replace_http_method() {
        let mut modules =
            TransportRpcModules { http: Some(create_test_module()), ..Default::default() };

        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_http(other_module.clone()).unwrap());

        assert!(modules.http.as_ref().unwrap().method("something").is_some());

        other_module.register_method("anything", |_, _, _| "fails").unwrap();
        assert!(modules.replace_http(other_module.clone()).unwrap());

        assert!(modules.http.as_ref().unwrap().method("anything").is_some());
    }

    #[test]
    fn test_replace_ws_method() {
        let mut modules =
            TransportRpcModules { ws: Some(create_test_module()), ..Default::default() };

        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_ws(other_module.clone()).unwrap());

        assert!(modules.ws.as_ref().unwrap().method("something").is_some());

        other_module.register_method("anything", |_, _, _| "fails").unwrap();
        assert!(modules.replace_ws(other_module.clone()).unwrap());

        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());
    }

    #[test]
    fn test_replace_configured() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),

            ..Default::default()
        };
        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_configured(other_module).unwrap());

        // Verify that the other_method was added
        assert!(modules.http.as_ref().unwrap().method("something").is_some());

        assert!(modules.ws.as_ref().unwrap().method("something").is_some());

        assert!(modules.http.as_ref().unwrap().method("anything").is_some());

        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());
    }

    #[test]
    fn test_add_or_replace_configured() {
        // Create a config that enables RpcNamespace::Eth for HTTP and WS, but NOT IPC
        let config = TransportRpcModuleConfig::default().with_http().with_ws();

        // Create HTTP module with an existing method (to test "replace")
        let mut http_module = RpcModule::new(());
        http_module.register_method("eth_existing", |_, _, _| "original").unwrap();

        // Create WS module with the same existing method
        let mut ws_module = RpcModule::new(());
        ws_module.register_method("eth_existing", |_, _, _| "original").unwrap();

        // Create IPC module (empty, to ensure no changes)

        // Set up TransportRpcModules with the config and modules
        let mut modules =
            TransportRpcModules { config, http: Some(http_module), ws: Some(ws_module) };

        // Create new methods: one to replace an existing method, one to add a new one
        let mut new_module = RpcModule::new(());
        new_module.register_method("eth_existing", |_, _, _| "replaced").unwrap(); // Replace
        new_module.register_method("eth_new", |_, _, _| "added").unwrap(); // Add
        let new_methods: Methods = new_module.into();

        // Call the function for RpcNamespace::Eth
        let result = modules.add_or_replace_configured(new_methods);
        assert!(result.is_ok(), "Function should succeed");

        // Verify HTTP: existing method still exists (replaced), new method added
        let http = modules.http.as_ref().unwrap();
        assert!(http.method("eth_existing").is_some());
        assert!(http.method("eth_new").is_some());

        // Verify WS: existing method still exists (replaced), new method added
        let ws = modules.ws.as_ref().unwrap();
        assert!(ws.method("eth_existing").is_some());
        assert!(ws.method("eth_new").is_some());

        // Verify IPC: no changes (Eth not configured for IPC)
    }
}
