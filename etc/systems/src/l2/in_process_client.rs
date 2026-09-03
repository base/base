//! In-process Base client node.
//!
//! Replaces Docker-based `ClientContainer` with an in-process node for faster tests.

use std::{any::Any, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::hex::ToHexExt;
use alloy_rpc_types_engine::JwtSecret;
use base_builder_core::test_utils::get_available_port;
use base_execution_chainspec::BaseChainSpec;
use base_execution_cli::{
    ExecutionUpgradeSignal, ExecutionUpgradeSignalConfig, ExecutionUpgradeSignalRuntimeExtension,
};
use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;
use base_flashblocks::FlashblocksConfig;
use base_flashblocks_node::FlashblocksExtension;
use base_node_core::args::RollupArgs;
use base_node_runner::{BaseNode, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_tx_forwarding::{TxForwardingConfig, TxForwardingExtension};
use base_txpool_rpc::{SendRawTransactionValidityExtension, TxPoolRpcConfig, TxPoolRpcExtension};
use base_txpool_tracing::{TxPoolExtension, TxpoolConfig};
use eyre::{Context, Result, eyre};
use reth_db::{ClientVersion, DatabaseEnv, init_db, mdbx::DatabaseArguments};
use reth_node_builder::{Node, NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::{
    args::{DatadirArgs, DiscoveryArgs, MetricArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
    exit::NodeExitFuture,
};
use reth_provider::providers::BlockchainProvider;
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
use tempfile::TempDir;
use tracing::warn;
use url::Url;

type BuiltExtensions = (Vec<Box<dyn BaseNodeExtension>>, Option<FlashblocksConfig>);

/// Source for the chain spec used to start an in-process client node.
#[derive(Debug, Clone)]
pub enum ChainSpecSource {
    /// Parse the chain spec from L2 genesis JSON content.
    GenesisJson(Vec<u8>),
    /// Use a pre-built chain specification.
    Parsed(Arc<BaseChainSpec>),
}

/// Configuration for starting an in-process client node.
#[derive(Debug)]
pub struct InProcessClientConfig {
    /// Chain specification source.
    pub chain_spec: ChainSpecSource,
    /// Existing caller-owned datadir. A temporary datadir is created when omitted.
    pub datadir: Option<PathBuf>,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// Builder HTTP RPC URL for rollup.sequencer.
    pub builder_rpc_url: String,
    /// Optional builder Flashblocks WebSocket URL.
    pub builder_flashblocks_url: Option<String>,
    /// Builder P2P enode for trusted-peers.
    pub builder_p2p_enode: String,
    /// Optional fixed HTTP RPC port (uses random if None).
    pub http_port: Option<u16>,
    /// Optional fixed WebSocket port (uses random if None).
    pub ws_port: Option<u16>,
    /// Optional fixed Auth RPC port (uses random if None).
    pub auth_port: Option<u16>,
    /// Optional fixed P2P port (uses random if None).
    pub p2p_port: Option<u16>,
    /// Optional fixed Prometheus metrics port (uses random if None).
    pub metrics_port: Option<u16>,
    /// Optional canonical block persistence threshold.
    pub persistence_threshold: Option<u64>,
    /// Optional number of unpersisted blocks allowed before Engine API intake is stalled.
    pub persistence_backpressure_threshold: Option<u64>,
    /// Optional transaction forwarding configuration.
    /// When set, the client will forward transactions to builder RPC endpoints.
    pub tx_forwarding_config: Option<TxForwardingConfig>,
    /// Whether to register the experimental validity transaction RPC.
    pub enable_experimental_validity_transactions: bool,
    /// Optional L1 upgrade signal configuration.
    ///
    /// When the mode applies at startup, the schedule is read from L1 and applied to the chain
    /// spec before the node starts, mirroring the standalone execution CLI. The runtime
    /// extension is installed for live polling (and, in runtime-admin mode, automatic
    /// re-application of observed L1 changes).
    pub upgrade_signal: Option<ExecutionUpgradeSignalConfig>,
    /// Additional node extensions installed after Base's built-in client extensions.
    ///
    /// Lets downstream consumers layer their own [`BaseNodeExtension`] — such as a custom RPC
    /// method — onto the standard in-process client wiring without forking this crate.
    pub extra_extensions: Vec<Box<dyn BaseNodeExtension>>,
}

/// In-process Base client node that syncs from a builder.
///
/// This replaces the Docker-based `ClientContainer` for faster test execution.
pub struct InProcessClient {
    http_api_addr: SocketAddr,
    ws_api_addr: SocketAddr,
    engine_addr: SocketAddr,
    metrics_addr: SocketAddr,
    chain_spec: Arc<BaseChainSpec>,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _runtime: Runtime,
    data_dir: PathBuf,
    _temp_dir: Option<TempDir>,
}

impl std::fmt::Debug for InProcessClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessClient")
            .field("http_api_addr", &self.http_api_addr)
            .field("ws_api_addr", &self.ws_api_addr)
            .field("engine_addr", &self.engine_addr)
            .field("metrics_addr", &self.metrics_addr)
            .finish_non_exhaustive()
    }
}

impl InProcessClient {
    /// Starts an in-process client node with the provided configuration.
    pub async fn start(config: InProcessClientConfig) -> Result<Self> {
        eyre::ensure!(
            config.upgrade_signal.is_none()
                || matches!(&config.chain_spec, ChainSpecSource::GenesisJson(_)),
            "upgrade_signal requires GenesisJson chain spec source"
        );

        let (data_dir, temp_dir) = Self::prepare_datadir(config.datadir.clone())?;
        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
        )
        .build()?;

        let chain_spec = match &config.chain_spec {
            ChainSpecSource::Parsed(chain_spec) => Arc::clone(chain_spec),
            ChainSpecSource::GenesisJson(genesis_json) => {
                let genesis: alloy_genesis::Genesis = serde_json::from_slice(genesis_json)
                    .map_err(|e| eyre!("Failed to parse genesis JSON: {}", e))?;
                let mut chain_spec = BaseChainSpec::try_from_genesis(genesis)
                    .wrap_err("Invalid genesis chain spec")?;

                // Mirror the standalone execution CLI: apply the L1 upgrade signal schedule to
                // the chain spec before startup when the configured mode applies at startup.
                if let Some(signal_config) = &config.upgrade_signal
                    && signal_config.signal_config.mode.applies_at_startup()
                {
                    ExecutionUpgradeSignal::apply_initial_signal_to_chain_spec(
                        signal_config,
                        &mut chain_spec,
                    )
                    .await
                    .wrap_err("Failed to apply upgrade signal to client chain spec")?;
                }

                Arc::new(chain_spec)
            }
        };

        let mut network_config = NetworkArgs {
            discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
            trusted_peers: vec![config.builder_p2p_enode.parse()?],
            ..NetworkArgs::default()
        };
        if let Some(port) = config.p2p_port {
            network_config.port = port;
        }

        std::fs::create_dir_all(&data_dir).wrap_err("Failed to create client datadir")?;
        let jwt_path = data_dir.join("jwt.hex");
        std::fs::write(&jwt_path, config.jwt_secret.as_bytes().encode_hex().as_bytes())
            .wrap_err("Failed to write JWT secret")?;

        let unique_ipc_path = format!(
            "/tmp/reth_client_api_{}_{}_{:?}.ipc",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            std::process::id(),
            std::thread::current().id()
        );

        let mut rpc_args =
            if config.http_port.is_some() || config.ws_port.is_some() || config.auth_port.is_some()
            {
                RpcServerArgs::default().with_http().with_auth_ipc().with_ws()
            } else {
                RpcServerArgs::default().with_unused_ports().with_http().with_auth_ipc().with_ws()
            };
        rpc_args.auth_ipc_path = unique_ipc_path;
        rpc_args.auth_jwtsecret = Some(jwt_path);
        rpc_args.rpc_eth_proof_window = 1_209_600;
        if let Some(port) = config.http_port {
            rpc_args.http_port = port;
        }
        if let Some(port) = config.ws_port {
            rpc_args.ws_port = port;
        }
        if let Some(port) = config.auth_port {
            rpc_args.auth_port = port;
        }

        // Configure rollup args with sequencer URL
        let rollup_args =
            RollupArgs { sequencer: Some(config.builder_rpc_url.clone()), ..Default::default() };

        let base_node = BaseNode::new(rollup_args.clone());

        let mut node_config = NodeConfig::new(Arc::clone(&chain_spec))
            .with_network(network_config)
            .with_rpc(rpc_args);
        if config.datadir.is_some() {
            node_config.debug.startup_sync_state_idle = true;
        }
        let metrics_addr = SocketAddr::new(
            std::net::Ipv4Addr::LOCALHOST.into(),
            config.metrics_port.unwrap_or_else(get_available_port),
        );
        node_config.metrics = MetricArgs { prometheus: Some(metrics_addr), ..Default::default() };
        if config.http_port.is_none()
            && config.ws_port.is_none()
            && config.auth_port.is_none()
            && config.p2p_port.is_none()
        {
            node_config = node_config.with_unused_ports();
        }
        if let Some(persistence_threshold) = config.persistence_threshold {
            node_config.engine.persistence_threshold = persistence_threshold;
        }
        if let Some(persistence_backpressure_threshold) = config.persistence_backpressure_threshold
        {
            node_config.engine.persistence_backpressure_threshold =
                Some(persistence_backpressure_threshold);
        }

        let datadir_path = MaybePlatformPath::<DataDirPath>::from(data_dir.clone());
        node_config = node_config
            .with_datadir_args(DatadirArgs { datadir: datadir_path, ..Default::default() });

        let db_path = node_config.datadir().db();
        let db = if config.datadir.is_some() {
            init_db(db_path, node_config.db.database_args())
                .wrap_err("Failed to open client database")?
        } else {
            Self::create_test_database(&db_path)?
        };

        let builder = NodeBuilder::new(node_config.clone())
            .with_database(db)
            .with_launch_context(runtime.clone())
            .with_types_and_provider::<BaseNode, BlockchainProvider<_>>()
            .with_components(base_node.components())
            .with_add_ons(base_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        let (mut extensions, flashblocks_config) = Self::build_extensions(&config)?;
        extensions.extend(config.extra_extensions);
        // Flashblocks extension must be installed last: it uses `replace_configured`, which
        // overwrites RPC methods (e.g. `eth_getTransactionCount`, `eth_subscribe`) that
        // built-in and caller-supplied extensions alike may register.
        extensions.push(Box::new(FlashblocksExtension::new(flashblocks_config)));
        let NodeHandle { node: node_handle, node_exit_future } = extensions
            .into_iter()
            .fold(NodeHooks::new(), |b, ext| ext.apply(b))
            .apply_to(builder)
            .launch()
            .await?;

        let http_api_addr = node_handle
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre!("HTTP RPC server failed to bind to address"))?;

        let ws_api_addr = node_handle
            .rpc_server_handle()
            .ws_local_addr()
            .ok_or_else(|| eyre!("Failed to get websocket api address"))?;

        let engine_addr = node_handle.auth_server_handle().local_addr();

        Ok(Self {
            http_api_addr,
            ws_api_addr,
            engine_addr,
            metrics_addr,
            chain_spec,
            _node_exit_future: node_exit_future,
            _node: Box::new(node_handle),
            _runtime: runtime,
            data_dir,
            _temp_dir: temp_dir,
        })
    }

    /// Returns the chain spec the node was started with, including any upgrade signal
    /// schedule applied at startup.
    pub const fn chain_spec(&self) -> &Arc<BaseChainSpec> {
        &self.chain_spec
    }

    fn prepare_datadir(datadir: Option<PathBuf>) -> Result<(PathBuf, Option<TempDir>)> {
        if let Some(path) = datadir {
            eyre::ensure!(path.is_dir(), "caller-owned client datadir does not exist");
            eyre::ensure!(
                path.join("db/mdbx.dat").is_file(),
                "caller-owned client datadir does not contain an existing database"
            );
            return Ok((path, None));
        }

        let temp_dir = TempDir::new().wrap_err("Failed to create temporary client datadir")?;
        let path = temp_dir.path().into();
        Ok((path, Some(temp_dir)))
    }

    /// Returns the HTTP RPC URL for the client.
    pub fn rpc_url(&self) -> Result<Url> {
        let url = Url::parse(&format!("http://{}", self.http_api_addr))
            .map_err(|e| eyre!("Failed to build HTTP URL: {}", e))?;
        Ok(url)
    }

    /// Returns the WebSocket URL for the client.
    pub fn ws_url(&self) -> Result<Url> {
        let url = Url::parse(&format!("ws://{}", self.ws_api_addr))
            .map_err(|e| eyre!("Failed to build WebSocket URL: {}", e))?;
        Ok(url)
    }

    /// Returns the Engine API URL (localhost).
    pub fn engine_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}", self.engine_addr))
            .map_err(|e| eyre!("Failed to build Engine URL: {}", e))
    }

    /// Returns the Prometheus metrics URL.
    pub fn metrics_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}/metrics", self.metrics_addr))
            .map_err(|e| eyre!("Failed to build metrics URL: {}", e))
    }

    /// Returns the execution datadir used by this client.
    pub fn datadir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Requests graceful node shutdown before releasing the private runtime.
    pub async fn shutdown(self) -> Result<()> {
        let shutdown = self
            ._runtime
            .initiate_graceful_shutdown()
            .map_err(|_| eyre!("client runtime shutdown channel closed"))?;
        shutdown.ignore_guard().await;
        match tokio::time::timeout(Duration::from_secs(60), self._node_exit_future).await {
            Ok(result) => result?,
            Err(error) => {
                warn!(error = %error, "forcing client runtime shutdown after graceful timeout");
            }
        }
        drop(self._node);
        let runtime = self._runtime;
        tokio::task::spawn_blocking(move || drop(runtime))
            .await
            .wrap_err("failed to release client runtime")?;
        Ok(())
    }

    /// Returns the Engine API URL for Docker containers using testcontainers host port exposure.
    pub fn host_engine_url(&self) -> String {
        format!("http://{}:{}", crate::host::host_address(), self.engine_addr.port())
    }

    /// Returns the engine port for host port exposure.
    pub const fn engine_port(&self) -> u16 {
        self.engine_addr.port()
    }

    /// Creates a test database with a 100 MB map size.
    fn create_test_database(path: &std::path::Path) -> Result<DatabaseEnv> {
        std::fs::create_dir_all(path).wrap_err("Failed to create client database directory")?;
        let args = DatabaseArguments::new(ClientVersion::default())
            .with_geometry_max_size(Some(100 * 1024 * 1024));
        init_db(path, args).wrap_err("Failed to create test database")
    }

    /// Builds the client node's built-in extensions, excluding [`FlashblocksExtension`].
    ///
    /// [`FlashblocksExtension`] must be installed last (see [`Self::start`]), since it uses
    /// `replace_configured` to overwrite RPC methods that other extensions may register.
    fn build_extensions(config: &InProcessClientConfig) -> Result<BuiltExtensions> {
        let mut extensions: Vec<Box<dyn BaseNodeExtension>> = Vec::new();

        // TxPool extension (tracing disabled for client)
        let flashblocks_config = config
            .builder_flashblocks_url
            .as_ref()
            .map(|value| {
                value
                    .parse::<Url>()
                    .map(|url| FlashblocksConfig::new(url, 3))
                    .map_err(|e| eyre!("Failed to parse flashblocks URL: {}", e))
            })
            .transpose()?;

        // TxPool RPC extension (management + status APIs)
        let txpool_rpc_config =
            TxPoolRpcConfig { sequencer_rpc: Some(config.builder_rpc_url.clone()) };
        extensions.push(Box::new(TxPoolRpcExtension::from_config(txpool_rpc_config)));

        // TxPool tracing extension (tracing disabled for client)
        let txpool_config = TxpoolConfig {
            tracing_enabled: false,
            tracing_logs_enabled: false,
            transaction_event_node_role: None,
            flashblocks_config: flashblocks_config.clone(),
        };
        extensions.push(Box::new(TxPoolExtension::new(txpool_config)));

        // TxForwarding extension (optional - forwards txs to builder RPC)
        if let Some(ref tx_fwd_config) = config.tx_forwarding_config {
            if config.enable_experimental_validity_transactions
                && tx_fwd_config.enabled
                && !tx_fwd_config.builder_urls.is_empty()
            {
                extensions.push(Box::new(SendRawTransactionValidityExtension::from_config(
                    DEFAULT_MAX_VALIDITY_PREDICATES,
                )));
            }
            extensions.push(Box::new(TxForwardingExtension::from_config(tx_fwd_config.clone())));
        }

        // Upgrade signal runtime extension (optional - live L1 schedule polling)
        if let Some(ref signal_config) = config.upgrade_signal {
            extensions.push(Box::new(ExecutionUpgradeSignalRuntimeExtension::from_config(
                signal_config.clone(),
            )));
        }

        Ok((extensions, flashblocks_config))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::InProcessClient;

    #[test]
    fn rejects_empty_caller_owned_datadir() {
        let datadir = TempDir::new().unwrap();

        let error = InProcessClient::prepare_datadir(Some(datadir.path().to_path_buf()))
            .expect_err("empty caller-owned datadir should be rejected");

        assert!(error.to_string().contains("does not contain an existing database"));
    }
}
