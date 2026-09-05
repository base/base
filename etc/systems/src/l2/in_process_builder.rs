//! In-process builder node for system tests.
//!
//! This module provides [`InProcessBuilder`], which spawns a real builder node in the
//! current process instead of using Docker containers. This enables faster test execution
//! and easier debugging while maintaining the same external interface as [`BuilderContainer`].

use core::net::{Ipv4Addr, SocketAddr};
use std::{any::Any, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::hex::ToHexExt;
use alloy_rpc_types_engine::JwtSecret;
use base_builder_core::{BuilderConfig, test_utils::get_available_port};
use base_builder_multiplex::MultiplexingServiceBuilder;
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    BasePooledTransaction, BuilderApiImpl, BuilderApiServer, DEFAULT_MAX_VALIDITY_PREDICATES,
};
use base_node_core::{args::RollupArgs, node::BasePoolBuilder};
use base_node_runner::{BaseNode, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use base_txpool_rpc::SendRawTransactionValidityExtension;
use eyre::{Result, WrapErr, eyre};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
};
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::{
    args::{DatadirArgs, MetricArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
    exit::NodeExitFuture,
};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
use tempfile::TempDir;
use tracing::warn;
use url::Url;

use crate::{config::BUILDER, setup::BUILDER_ENODE_ID};

/// Configuration for starting an in-process builder.
#[derive(Debug)]
pub struct InProcessBuilderConfig {
    /// Pre-built chain specification.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Existing caller-owned datadir. A temporary datadir is created when omitted.
    pub datadir: Option<PathBuf>,
    /// JWT secret hex for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// Optional fixed HTTP RPC port (uses random if None).
    pub http_port: Option<u16>,
    /// Optional fixed WebSocket port (uses random if None).
    pub ws_port: Option<u16>,
    /// Optional fixed Auth RPC port (uses random if None).
    pub auth_port: Option<u16>,
    /// Optional fixed P2P port (uses random if None).
    pub p2p_port: Option<u16>,
    /// Optional fixed Flashblocks port (uses random if None).
    pub flashblocks_port: Option<u16>,
    /// Optional fixed Prometheus metrics port (uses random if None).
    pub metrics_port: Option<u16>,
    /// Whether to accept experimental validity-bearing transactions and expose
    /// `base_sendRawTransactionValidity`.
    pub enable_experimental_validity_transactions: bool,
    /// Additional node extensions installed after the builder's built-in RPC wiring.
    ///
    /// Lets downstream consumers layer their own [`BaseNodeExtension`] onto the standard
    /// in-process builder wiring without forking this crate.
    pub extra_extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Interval used by the payload builder.
    pub block_time: Duration,
    /// Optional canonical block persistence threshold.
    pub persistence_threshold: Option<u64>,
    /// Optional pending/basefee/queued transaction count limit for benchmark workloads.
    pub txpool_max_transactions: Option<usize>,
    /// Optional pending/basefee/queued transaction size limit in megabytes.
    pub txpool_max_size_mb: Option<usize>,
    /// Optional maximum number of transaction slots retained per sender.
    pub txpool_max_account_slots: Option<usize>,
}

impl InProcessBuilderConfig {
    /// Parses L2 genesis JSON into a chain specification.
    pub fn chain_spec_from_genesis_json(genesis_json: &[u8]) -> Result<Arc<BaseChainSpec>> {
        let genesis: alloy_genesis::Genesis =
            serde_json::from_slice(genesis_json).wrap_err("Invalid genesis JSON")?;
        Ok(Arc::new(
            BaseChainSpec::try_from_genesis(genesis).wrap_err("Invalid genesis chain spec")?,
        ))
    }
}

/// An in-process builder node that replaces Docker-based `BuilderContainer`.
///
/// This spawns a real builder node within the current process, binding to dynamic ports.
/// Docker containers (like consensus nodes) can connect via `host.docker.internal`.
pub struct InProcessBuilder {
    http_api_addr: SocketAddr,
    ws_api_addr: SocketAddr,
    engine_addr: SocketAddr,
    metrics_addr: SocketAddr,
    flashblocks_port: u16,
    p2p_port: u16,
    data_dir: PathBuf,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _runtime: Runtime,
    _temp_dir: Option<TempDir>,
}

impl std::fmt::Debug for InProcessBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessBuilder")
            .field("http_api_addr", &self.http_api_addr)
            .field("ws_api_addr", &self.ws_api_addr)
            .field("engine_addr", &self.engine_addr)
            .field("metrics_addr", &self.metrics_addr)
            .field("flashblocks_port", &self.flashblocks_port)
            .field("p2p_port", &self.p2p_port)
            .finish_non_exhaustive()
    }
}

impl InProcessBuilder {
    /// Starts an in-process builder node with the provided configuration.
    pub async fn start(config: InProcessBuilderConfig) -> Result<Self> {
        clear_otel_env_vars();

        let (data_path, temp_dir) = Self::prepare_datadir(config.datadir.clone())?;
        let jwt_path = data_path.join("jwt.hex");

        std::fs::create_dir_all(&data_path).wrap_err("Failed to create data directory")?;
        std::fs::write(&jwt_path, config.jwt_secret.as_bytes().encode_hex().as_bytes())
            .wrap_err("Failed to write JWT secret")?;

        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio::runtime::Handle::current())),
        )
        .build()?;

        let chain_spec = Arc::clone(&config.chain_spec);

        let flashblocks_port = config.flashblocks_port.unwrap_or_else(get_available_port);
        let metrics_addr = SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            config.metrics_port.unwrap_or_else(get_available_port),
        );
        let builder_config = BuilderConfig {
            block_time: config.block_time,
            block_time_leeway: Duration::from_secs(20),
            flashblocks_ws_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), flashblocks_port),
            flashblocks_interval: Duration::from_millis(200),
            ..Default::default()
        };

        let flashblocks_ws_addr = builder_config.flashblocks_ws_addr;

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();

        let rollup_args = RollupArgs::default();
        let base_node = BaseNode::new(rollup_args.clone());

        let addons: base_node_runner::BaseAddOns<
            _,
            base_execution_rpc::BaseEthApiBuilder,
            base_node_core::BasePayloadValidatorBuilder,
        > = base_node
            .add_ons_builder()
            .with_sequencer(rollup_args.sequencer.clone())
            .with_da_config(da_config)
            .with_gas_limit_config(gas_limit_config)
            .build();

        let mut node_config = create_node_config(chain_spec, &data_path, &jwt_path, &config)?;
        node_config.metrics = MetricArgs { prometheus: Some(metrics_addr), ..Default::default() };
        let db_path = node_config.datadir().db();
        let db = if config.datadir.is_some() {
            init_db(db_path, node_config.db.database_args())
                .wrap_err("Failed to open builder database")?
        } else {
            create_test_db(&db_path)?
        };
        let p2p_port = node_config.network.port;

        let accept_validity_transactions = config.enable_experimental_validity_transactions;
        let extra_extensions = config.extra_extensions;
        let mut hooks = NodeHooks::new();
        if accept_validity_transactions {
            hooks = Box::new(SendRawTransactionValidityExtension::from_config(
                DEFAULT_MAX_VALIDITY_PREDICATES,
            ))
            .apply(hooks);
        }
        // Reth's `extend_rpc_modules` is a single-slot hook that silently replaces whatever was
        // registered before it, and `NodeHooks::apply_to` claims that slot for every extension
        // RPC module. Registering the builder API here instead keeps both in one closure.
        let hooks = hooks.add_rpc_module(move |ctx| {
            let api =
                BuilderApiImpl::<_, base_execution_txpool::TransactionValidity>::with_extensions(
                    ctx.pool().clone(),
                    accept_validity_transactions,
                    DEFAULT_MAX_VALIDITY_PREDICATES,
                );
            ctx.modules.merge_configured(api.into_rpc())?;
            Ok(())
        });

        let node_builder = NodeBuilder::new(node_config.clone())
            .with_database(db)
            .with_launch_context(runtime.clone())
            .with_types::<BaseNode>();

        let launched = extra_extensions
            .into_iter()
            .fold(hooks, |hooks, ext| ext.apply(hooks))
            .apply_to(
                node_builder
                    .with_components(
                        base_node
                            .components()
                            .pool(pool_component(&rollup_args))
                            .payload(MultiplexingServiceBuilder::new(builder_config)),
                    )
                    .with_add_ons(addons)
                    .on_component_initialized(move |_ctx| Ok(())),
            )
            .launch()
            .await;

        let NodeHandle { node: node_handle, node_exit_future } =
            launched.wrap_err("Failed to launch builder node")?;

        let http_api_addr = node_handle
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre!("HTTP RPC server failed to bind to address"))?;

        let ws_api_addr = node_handle
            .rpc_server_handle()
            .ws_local_addr()
            .ok_or_else(|| eyre!("WebSocket RPC server failed to bind to address"))?;

        let engine_addr = node_handle.auth_server_handle().local_addr();

        Ok(Self {
            http_api_addr,
            ws_api_addr,
            engine_addr,
            metrics_addr,
            flashblocks_port: flashblocks_ws_addr.port(),
            p2p_port,
            data_dir: data_path,
            _node_exit_future: node_exit_future,
            _node: Box::new(node_handle),
            _runtime: runtime,
            _temp_dir: temp_dir,
        })
    }

    fn prepare_datadir(datadir: Option<PathBuf>) -> Result<(PathBuf, Option<TempDir>)> {
        if let Some(path) = datadir {
            eyre::ensure!(path.is_dir(), "caller-owned builder datadir does not exist");
            eyre::ensure!(
                path.join("db/mdbx.dat").is_file(),
                "caller-owned builder datadir does not contain an existing database"
            );
            return Ok((path, None));
        }

        let temp_dir = TempDir::new().wrap_err("Failed to create temporary builder datadir")?;
        let path = temp_dir.path().into();
        Ok((path, Some(temp_dir)))
    }

    /// Returns the HTTP RPC URL (`localhost:actual_port`).
    pub fn rpc_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}", self.http_api_addr)).wrap_err("Failed to parse RPC URL")
    }

    /// Returns the Engine API URL.
    pub fn engine_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}", self.engine_addr)).wrap_err("Failed to parse Engine URL")
    }

    /// Returns the WebSocket URL.
    pub fn ws_url(&self) -> Result<Url> {
        Url::parse(&format!("ws://{}", self.ws_api_addr)).wrap_err("Failed to parse WebSocket URL")
    }

    /// Returns the Flashblocks WebSocket URL.
    pub fn flashblocks_url(&self) -> String {
        format!("ws://127.0.0.1:{}/", self.flashblocks_port)
    }

    /// Returns the Prometheus metrics URL.
    pub fn metrics_url(&self) -> Result<Url> {
        Url::parse(&format!("http://{}/metrics", self.metrics_addr))
            .wrap_err("Failed to parse metrics URL")
    }

    /// Returns the P2P enode URL with actual bound port.
    pub fn p2p_enode(&self) -> String {
        format!("enode://{BUILDER_ENODE_ID}@127.0.0.1:{}", self.p2p_port)
    }

    /// Returns the execution datadir used by this builder.
    pub fn datadir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Requests graceful node shutdown before releasing the private runtime.
    pub async fn shutdown(self) -> Result<()> {
        let shutdown = self
            ._runtime
            .initiate_graceful_shutdown()
            .map_err(|_| eyre!("builder runtime shutdown channel closed"))?;
        shutdown.ignore_guard().await;
        match tokio::time::timeout(Duration::from_secs(60), self._node_exit_future).await {
            Ok(result) => result?,
            Err(error) => {
                warn!(error = %error, "forcing builder runtime shutdown after graceful timeout");
            }
        }
        drop(self._node);
        let runtime = self._runtime;
        tokio::task::spawn_blocking(move || drop(runtime))
            .await
            .wrap_err("failed to release builder runtime")?;
        Ok(())
    }

    /// Returns the Engine URL for Docker containers using testcontainers host port exposure.
    pub fn host_engine_url(&self) -> String {
        format!("http://{}:{}", crate::host::host_address(), self.engine_addr.port())
    }

    /// Returns the engine port for host port exposure.
    pub const fn engine_port(&self) -> u16 {
        self.engine_addr.port()
    }

    /// Returns the HTTP RPC URL for Docker containers using testcontainers host port exposure.
    pub fn host_rpc_url(&self) -> String {
        format!("http://{}:{}", crate::host::host_address(), self.http_api_addr.port())
    }

    /// Returns the HTTP RPC port for host port exposure.
    pub const fn rpc_port(&self) -> u16 {
        self.http_api_addr.port()
    }

    /// Returns the P2P enode URL for Docker containers using testcontainers host port exposure.
    pub fn host_p2p_enode(&self) -> String {
        format!("enode://{BUILDER_ENODE_ID}@{}:{}", crate::host::host_address(), self.p2p_port)
    }

    /// Returns the Flashblocks URL for Docker containers using testcontainers host port exposure.
    pub fn host_flashblocks_url(&self) -> String {
        format!("ws://{}:{}/", crate::host::host_address(), self.flashblocks_port)
    }
}

fn clear_otel_env_vars() {
    for key in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_LOGS_EXPORTER",
        "OTEL_METRICS_EXPORTER",
        "OTEL_TRACES_EXPORTER",
        "OTEL_SDK_DISABLED",
    ] {
        // SAFETY: We're in a test environment where env var mutation is acceptable
        unsafe { std::env::remove_var(key) };
    }
}

fn create_node_config(
    chain_spec: Arc<BaseChainSpec>,
    data_path: &std::path::Path,
    jwt_path: &std::path::Path,
    config: &InProcessBuilderConfig,
) -> Result<NodeConfig<BaseChainSpec>> {
    let mut rpc =
        if config.http_port.is_some() || config.ws_port.is_some() || config.auth_port.is_some() {
            RpcServerArgs::default().with_http().with_ws()
        } else {
            RpcServerArgs::default().with_unused_ports().with_http().with_ws()
        };

    rpc.http_addr = Ipv4Addr::LOCALHOST.into();
    rpc.ws_addr = Ipv4Addr::LOCALHOST.into();
    rpc.auth_jwtsecret = Some(jwt_path.to_path_buf());
    // Match docker-compose `--rpc.eth-proof-window=1209600` (reth default is 0).
    rpc.rpc_eth_proof_window = 1_209_600;

    if let Some(port) = config.http_port {
        rpc.http_port = port;
    }
    if let Some(port) = config.ws_port {
        rpc.ws_port = port;
    }
    if let Some(port) = config.auth_port {
        rpc.auth_port = port;
    }

    rpc.http_api = Some(
        "admin,eth,web3,net,rpc,debug,txpool,miner"
            .parse()
            .wrap_err("Failed to parse HTTP API modules")?,
    );
    rpc.ws_api = Some(
        "admin,eth,web3,net,rpc,debug,txpool,miner"
            .parse()
            .wrap_err("Failed to parse WS API modules")?,
    );

    let mut network = if config.p2p_port.is_some() {
        NetworkArgs::default()
    } else {
        NetworkArgs::default().with_unused_ports()
    };
    network.p2p_secret_key_hex = Some(BUILDER.private_key);
    network.discovery.disable_discovery = true;
    if let Some(port) = config.p2p_port {
        network.port = port;
    }

    let datadir = DatadirArgs {
        datadir: MaybePlatformPath::<DataDirPath>::from(data_path.to_path_buf()),
        static_files_path: None,
        rocksdb_path: None,
        pprof_dumps_path: None,
    };

    let mut node_config = NodeConfig::<BaseChainSpec>::new(chain_spec)
        .with_datadir_args(datadir)
        .with_rpc(rpc)
        .with_network(network);

    if let Some(persistence_threshold) = config.persistence_threshold {
        node_config.engine.persistence_threshold = persistence_threshold;
    }
    if let Some(max_transactions) = config.txpool_max_transactions {
        node_config.txpool.pending_max_count = max_transactions;
        node_config.txpool.basefee_max_count = max_transactions;
        node_config.txpool.queued_max_count = max_transactions;
    }
    if let Some(max_size_mb) = config.txpool_max_size_mb {
        node_config.txpool.pending_max_size = max_size_mb;
        node_config.txpool.basefee_max_size = max_size_mb;
        node_config.txpool.queued_max_size = max_size_mb;
    }
    if let Some(max_account_slots) = config.txpool_max_account_slots {
        node_config.txpool.max_account_slots = max_account_slots;
    }

    if config.http_port.is_none()
        && config.ws_port.is_none()
        && config.auth_port.is_none()
        && config.p2p_port.is_none()
    {
        node_config = node_config.with_unused_ports();
    }

    Ok(node_config)
}

fn create_test_db(db_path: &std::path::Path) -> Result<DatabaseEnv> {
    std::fs::create_dir_all(db_path).wrap_err("Failed to create db directory")?;

    let db = init_db(
        db_path,
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            .with_geometry_max_size(Some(4 * MEGABYTE))
            .with_growth_step(Some(4 * KILOBYTE)),
    )
    .wrap_err("Failed to initialize database")?;

    Ok(db)
}

fn pool_component(_rollup_args: &RollupArgs) -> BasePoolBuilder<BasePooledTransaction> {
    BasePoolBuilder::<BasePooledTransaction>::default()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::InProcessBuilder;

    #[test]
    fn retains_caller_owned_datadir() {
        let parent = TempDir::new().unwrap();
        let datadir = parent.path().join("builder");
        std::fs::create_dir_all(datadir.join("db")).unwrap();
        std::fs::write(datadir.join("db/mdbx.dat"), []).unwrap();
        let sentinel = datadir.join("caller-owned");
        std::fs::write(&sentinel, []).unwrap();

        let (actual, owner) = InProcessBuilder::prepare_datadir(Some(datadir.clone())).unwrap();
        assert_eq!(actual, datadir);
        assert!(owner.is_none());
        drop(owner);

        assert!(sentinel.exists());
    }

    #[test]
    fn rejects_empty_caller_owned_datadir() {
        let datadir = TempDir::new().unwrap();

        let error = InProcessBuilder::prepare_datadir(Some(datadir.path().to_path_buf()))
            .expect_err("empty caller-owned datadir should be rejected");

        assert!(error.to_string().contains("does not contain an existing database"));
    }

    #[test]
    fn removes_temporary_datadir() {
        let (datadir, owner) = InProcessBuilder::prepare_datadir(None).unwrap();
        assert!(datadir.exists());

        drop(owner);

        assert!(!datadir.exists());
    }
}
