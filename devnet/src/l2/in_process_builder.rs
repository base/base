//! In-process builder node for system tests.
//!
//! This module provides [`InProcessBuilder`], which spawns a real builder node in the
//! current process instead of using Docker containers. This enables faster test execution
//! and easier debugging while maintaining the same external interface as [`BuilderContainer`].

use core::net::{Ipv4Addr, SocketAddr};
use std::{any::Any, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::hex::ToHexExt;
use alloy_rpc_types_engine::JwtSecret;
use base_builder_core::{
    BuilderConfig, FlashblocksConfig, FlashblocksServiceBuilder, test_utils::get_available_port,
};
use base_execution_chainspec::OpChainSpec;
use base_execution_txpool::OpPooledTransaction;
use base_node_core::{args::RollupArgs, node::OpPoolBuilder};
use base_node_runner::BaseNode;
use eyre::{Result, WrapErr, eyre};
use nanoid::nanoid;
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::TempDatabase,
};
use reth_node_builder::{NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::{
    args::{DatadirArgs, NetworkArgs, RpcServerArgs},
    dirs::{DataDirPath, MaybePlatformPath},
    exit::NodeExitFuture,
};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};
use tracing::warn;
use url::Url;

use crate::{config::BUILDER, setup::BUILDER_ENODE_ID};

/// Configuration for starting an in-process builder.
#[derive(Debug, Clone)]
pub struct InProcessBuilderConfig {
    /// L2 genesis JSON content.
    pub genesis_json: Vec<u8>,
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
}

/// An in-process builder node that replaces Docker-based `BuilderContainer`.
///
/// This spawns a real builder node within the current process, binding to dynamic ports.
/// Docker containers (like op-node) can connect via `host.docker.internal`.
pub struct InProcessBuilder {
    http_api_addr: SocketAddr,
    ws_api_addr: SocketAddr,
    engine_addr: SocketAddr,
    flashblocks_port: u16,
    p2p_port: u16,
    data_dir: PathBuf,
    _node_exit_future: NodeExitFuture,
    _node: Box<dyn Any + Sync + Send>,
    _runtime: Runtime,
}

impl Drop for InProcessBuilder {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.data_dir) {
            warn!(dir = ?self.data_dir, error = %e, "Failed to remove temp data directory");
        }
    }
}

impl std::fmt::Debug for InProcessBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessBuilder")
            .field("http_api_addr", &self.http_api_addr)
            .field("ws_api_addr", &self.ws_api_addr)
            .field("engine_addr", &self.engine_addr)
            .field("flashblocks_port", &self.flashblocks_port)
            .field("p2p_port", &self.p2p_port)
            .finish_non_exhaustive()
    }
}

impl InProcessBuilder {
    /// Starts an in-process builder node with the provided configuration.
    pub async fn start(config: InProcessBuilderConfig) -> Result<Self> {
        clear_otel_env_vars();

        let tempdir = std::env::temp_dir();
        let random_id = nanoid!();
        let data_path = tempdir.join(format!("in-process-builder.{random_id}"));
        let jwt_path = data_path.join("jwt.hex");

        std::fs::create_dir_all(&data_path).wrap_err("Failed to create data directory")?;
        std::fs::write(&jwt_path, config.jwt_secret.as_bytes().encode_hex().as_bytes())
            .wrap_err("Failed to write JWT secret")?;

        let runtime = RuntimeBuilder::new(RuntimeConfig::default()).build()?;

        let chain_spec = parse_genesis(&config.genesis_json)?;

        let flashblocks_port = config.flashblocks_port.unwrap_or_else(get_available_port);
        let builder_config = BuilderConfig {
            block_time: Duration::from_millis(2000),
            flashblocks: FlashblocksConfig {
                ws_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), flashblocks_port),
                interval: Duration::from_millis(200),
                disable_state_root: true,
                compute_state_root_on_finalize: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let flashblocks_ws_addr = builder_config.flashblocks.ws_addr;

        let da_config = builder_config.da_config.clone();
        let gas_limit_config = builder_config.gas_limit_config.clone();

        let rollup_args = RollupArgs::default();

        let base_node = BaseNode::new(rollup_args.clone());

        let addons: base_node_runner::BaseAddOns<
            _,
            base_execution_rpc::OpEthApiBuilder,
            base_node_core::OpEngineValidatorBuilder,
        > = base_node
            .add_ons_builder()
            .with_sequencer(rollup_args.sequencer.clone())
            .with_da_config(da_config)
            .with_gas_limit_config(gas_limit_config)
            .build();

        let (db, db_path) = create_test_db(&data_path)?;

        let node_config = create_node_config(chain_spec, &data_path, &jwt_path, &config)?;
        let p2p_port = node_config.network.port;

        let node_builder = NodeBuilder::new(node_config.clone())
            .with_database(db)
            .with_launch_context(runtime.clone())
            .with_types::<BaseNode>()
            .with_components(
                base_node
                    .components()
                    .pool(pool_component(&rollup_args))
                    .payload(FlashblocksServiceBuilder(builder_config)),
            )
            .with_add_ons(addons)
            .on_component_initialized(move |_ctx| Ok(()));

        let NodeHandle { node: node_handle, node_exit_future } =
            node_builder.launch().await.wrap_err("Failed to launch builder node")?;

        let http_api_addr = node_handle
            .rpc_server_handle()
            .http_local_addr()
            .ok_or_else(|| eyre!("HTTP RPC server failed to bind to address"))?;

        let ws_api_addr = node_handle
            .rpc_server_handle()
            .ws_local_addr()
            .ok_or_else(|| eyre!("WebSocket RPC server failed to bind to address"))?;

        let engine_addr = node_handle.auth_server_handle().local_addr();

        // Delete db_path since we use data_path as the main cleanup path
        drop(db_path);

        Ok(Self {
            http_api_addr,
            ws_api_addr,
            engine_addr,
            flashblocks_port: flashblocks_ws_addr.port(),
            p2p_port,
            data_dir: data_path,
            _node_exit_future: node_exit_future,
            _node: Box::new(node_handle),
            _runtime: runtime,
        })
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

    /// Returns the P2P enode URL with actual bound port.
    pub fn p2p_enode(&self) -> String {
        format!("enode://{BUILDER_ENODE_ID}@127.0.0.1:{}", self.p2p_port)
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

fn parse_genesis(genesis_json: &[u8]) -> Result<Arc<OpChainSpec>> {
    let genesis: alloy_genesis::Genesis =
        serde_json::from_slice(genesis_json).wrap_err("Invalid genesis JSON")?;
    Ok(Arc::new(OpChainSpec::from_genesis(genesis)))
}

fn create_node_config(
    chain_spec: Arc<OpChainSpec>,
    data_path: &std::path::Path,
    jwt_path: &std::path::Path,
    config: &InProcessBuilderConfig,
) -> Result<NodeConfig<OpChainSpec>> {
    let mut rpc =
        if config.http_port.is_some() || config.ws_port.is_some() || config.auth_port.is_some() {
            RpcServerArgs::default().with_http().with_ws()
        } else {
            RpcServerArgs::default().with_unused_ports().with_http().with_ws()
        };

    rpc.http_addr = Ipv4Addr::LOCALHOST.into();
    rpc.ws_addr = Ipv4Addr::LOCALHOST.into();
    rpc.auth_jwtsecret = Some(jwt_path.to_path_buf());

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

    let mut node_config = NodeConfig::<OpChainSpec>::new(chain_spec)
        .with_datadir_args(datadir)
        .with_rpc(rpc)
        .with_network(network);

    if config.http_port.is_none()
        && config.ws_port.is_none()
        && config.auth_port.is_none()
        && config.p2p_port.is_none()
    {
        node_config = node_config.with_unused_ports();
    }

    Ok(node_config)
}

fn create_test_db(
    data_path: &std::path::Path,
) -> Result<(Arc<TempDatabase<DatabaseEnv>>, PathBuf)> {
    let db_path = data_path.join("db");
    std::fs::create_dir_all(&db_path).wrap_err("Failed to create db directory")?;

    let db = init_db(
        db_path.as_path(),
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            .with_geometry_max_size(Some(4 * MEGABYTE))
            .with_growth_step(Some(4 * KILOBYTE)),
    )
    .wrap_err("Failed to initialize database")?;

    Ok((Arc::new(TempDatabase::new(db, db_path.clone())), db_path))
}

fn pool_component(_rollup_args: &RollupArgs) -> OpPoolBuilder<OpPooledTransaction> {
    OpPoolBuilder::<OpPooledTransaction>::default()
}
