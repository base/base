//! op-node container for L2 consensus.

use alloy_primitives::B256;
use alloy_rpc_types_engine::JwtSecret;
use eyre::{Result, WrapErr, eyre};
use hex::ToHex;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use url::Url;

use crate::{
    containers::{
        L2_CLIENT_OP_NODE_NAME, L2_OP_NODE_NAME, L2_OP_NODE_P2P_PORT, L2_OP_NODE_RPC_PORT,
    },
    host::with_host_port_if_needed,
    images::OP_NODE_IMAGE,
    l2::L2ContainerConfig,
    network::{ensure_network_exists, network_name},
    setup::BUILDER_LIBP2P_PEER_ID,
    unique_name,
};

const RPC_PORT: u16 = 9545;
const P2P_PORT: u16 = 9222;
const ROLLUP_CONFIG_PATH: &str = "/genesis/rollup.json";
const L1_GENESIS_PATH: &str = "/genesis/l1-genesis.json";
const JWT_PATH: &str = "/genesis/jwt.hex";
const P2P_KEY_PATH: &str = "/genesis/p2p.key";

/// Configuration for starting a sequencer op-node container.
#[derive(Debug, Clone)]
pub struct OpNodeConfig {
    /// Rollup configuration JSON.
    pub rollup_config: Vec<u8>,
    /// L1 genesis JSON (for chain spec).
    pub l1_genesis: Vec<u8>,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// P2P private key for libp2p identity.
    pub p2p_key: B256,
    /// Sequencer private key for block signing.
    pub sequencer_key: B256,
    /// L1 RPC URL.
    pub l1_rpc_url: String,
    /// L1 beacon API URL.
    pub l1_beacon_url: String,
    /// L2 engine API URL.
    pub l2_engine_url: String,
    /// L2 engine port on host (for testcontainers host port exposure).
    pub l2_engine_port: u16,
}

/// Configuration for starting a follower op-node container.
#[derive(Debug, Clone)]
pub struct OpNodeFollowerConfig {
    /// Rollup configuration JSON.
    pub rollup_config: Vec<u8>,
    /// L1 genesis JSON (for chain spec).
    pub l1_genesis: Vec<u8>,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// L1 RPC URL.
    pub l1_rpc_url: String,
    /// L1 beacon API URL.
    pub l1_beacon_url: String,
    /// L2 engine API URL.
    pub l2_engine_url: String,
    /// L2 engine port on host (for testcontainers host port exposure).
    pub l2_engine_port: u16,
    /// Builder op-node container name for P2P static peer.
    pub builder_op_node_name: String,
    /// Builder op-node libp2p peer ID for P2P sync.
    pub builder_op_node_peer_id: String,
}

/// Running sequencer op-node container.
#[derive(Debug)]
pub struct OpNodeContainer {
    container: ContainerAsync<GenericImage>,
    name: String,
    libp2p_peer_id: String,
}

impl OpNodeContainer {
    /// Starts a sequencer op-node container with the provided configuration.
    pub async fn start(
        config: OpNodeConfig,
        container_config: Option<&L2ContainerConfig>,
    ) -> Result<Self> {
        ensure_network_exists()?;

        let (image_name, image_tag) =
            OP_NODE_IMAGE.split_once(':').ok_or_else(|| eyre!("op-node image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_entrypoint("op-node")
            .with_exposed_port(RPC_PORT.tcp())
            .with_exposed_port(P2P_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Starting JSON-RPC server"));

        let name = if container_config.is_some_and(|c| c.use_stable_names) {
            L2_OP_NODE_NAME.to_string()
        } else {
            unique_name(L2_OP_NODE_NAME)
        };

        let network = container_config
            .and_then(|c| c.network_name.clone())
            .unwrap_or_else(|| network_name().to_string());

        let base_container = image
            .with_container_name(&name)
            .with_network(&network)
            .with_cmd(sequencer_args(&config))
            .with_copy_to(ROLLUP_CONFIG_PATH, config.rollup_config)
            .with_copy_to(L1_GENESIS_PATH, config.l1_genesis)
            .with_copy_to(JWT_PATH, config.jwt_secret.as_bytes().to_vec())
            .with_copy_to(P2P_KEY_PATH, config.p2p_key.to_vec());

        let mut container_builder = with_host_port_if_needed(base_container, config.l2_engine_port);

        if let Some(rpc_port) = container_config.and_then(|c| c.op_node_rpc_port) {
            container_builder = container_builder.with_mapped_port(rpc_port, RPC_PORT.tcp());
        }
        if let Some(p2p_port) = container_config.and_then(|c| c.op_node_p2p_port) {
            container_builder = container_builder.with_mapped_port(p2p_port, P2P_PORT.tcp());
        }

        let container =
            container_builder.start().await.wrap_err("Failed to start op-node container")?;

        Ok(Self { container, name, libp2p_peer_id: BUILDER_LIBP2P_PEER_ID.to_string() })
    }

    /// Returns the RPC URL for the container (host-accessible).
    pub async fn rpc_url(&self) -> Result<Url> {
        let host = self.container.get_host().await.wrap_err("Failed to resolve container host")?;
        let host_port = self
            .container
            .get_host_port_ipv4(RPC_PORT)
            .await
            .wrap_err("Failed to resolve container port")?;
        let url = Url::parse(&format!("http://{host}:{host_port}"))
            .wrap_err("Failed to build container URL")?;
        Ok(url)
    }

    /// Returns the internal RPC URL for inter-container communication.
    pub fn internal_rpc_url(&self) -> String {
        format!("http://{}:{}", self.name, L2_OP_NODE_RPC_PORT)
    }

    /// Returns the internal P2P multiaddr for inter-container communication.
    pub fn internal_p2p_addr(&self) -> String {
        format!("/dns4/{}/tcp/{}/p2p/{}", self.name, L2_OP_NODE_P2P_PORT, self.libp2p_peer_id)
    }

    /// Returns the libp2p peer ID of this op-node.
    pub fn libp2p_peer_id(&self) -> &str {
        &self.libp2p_peer_id
    }

    /// Returns the container name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Running follower op-node container.
#[derive(Debug)]
pub struct OpNodeFollowerContainer {
    container: ContainerAsync<GenericImage>,
    #[allow(dead_code)]
    name: String,
}

impl OpNodeFollowerContainer {
    /// Starts a follower op-node container with the provided configuration.
    pub async fn start(
        config: OpNodeFollowerConfig,
        container_config: Option<&L2ContainerConfig>,
    ) -> Result<Self> {
        ensure_network_exists()?;

        let (image_name, image_tag) =
            OP_NODE_IMAGE.split_once(':').ok_or_else(|| eyre!("op-node image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_entrypoint("op-node")
            .with_exposed_port(RPC_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Starting JSON-RPC server"));

        let name = if container_config.is_some_and(|c| c.use_stable_names) {
            L2_CLIENT_OP_NODE_NAME.to_string()
        } else {
            unique_name(L2_CLIENT_OP_NODE_NAME)
        };

        let network = container_config
            .and_then(|c| c.network_name.clone())
            .unwrap_or_else(|| network_name().to_string());

        let base_container = image
            .with_container_name(&name)
            .with_network(&network)
            .with_cmd(follower_args(&config))
            .with_copy_to(ROLLUP_CONFIG_PATH, config.rollup_config)
            .with_copy_to(L1_GENESIS_PATH, config.l1_genesis)
            .with_copy_to(JWT_PATH, config.jwt_secret.as_bytes().to_vec());

        let mut container_builder = with_host_port_if_needed(base_container, config.l2_engine_port);

        if let Some(rpc_port) = container_config.and_then(|c| c.op_node_follower_rpc_port) {
            container_builder = container_builder.with_mapped_port(rpc_port, RPC_PORT.tcp());
        }

        let container = container_builder
            .start()
            .await
            .wrap_err("Failed to start follower op-node container")?;

        Ok(Self { container, name })
    }

    /// Returns the RPC URL for the container (host-accessible).
    pub async fn rpc_url(&self) -> Result<Url> {
        let host = self.container.get_host().await.wrap_err("Failed to resolve container host")?;
        let host_port = self
            .container
            .get_host_port_ipv4(RPC_PORT)
            .await
            .wrap_err("Failed to resolve container port")?;
        let url = Url::parse(&format!("http://{host}:{host_port}"))
            .wrap_err("Failed to build container URL")?;
        Ok(url)
    }
}

fn sequencer_args(config: &OpNodeConfig) -> Vec<String> {
    let sequencer_key: String = config.sequencer_key.encode_hex();
    vec![
        format!("--rollup.config={ROLLUP_CONFIG_PATH}"),
        format!("--rollup.l1-chain-config={L1_GENESIS_PATH}"),
        format!("--l1={}", config.l1_rpc_url),
        format!("--l1.beacon={}", config.l1_beacon_url),
        format!("--l2={}", config.l2_engine_url),
        format!("--l2.jwt-secret={JWT_PATH}"),
        "--sequencer.enabled".to_string(),
        "--sequencer.l1-confs=0".to_string(),
        format!("--p2p.priv.path={P2P_KEY_PATH}"),
        format!("--p2p.sequencer.key={sequencer_key}"),
        format!("--p2p.listen.tcp={P2P_PORT}"),
        format!("--p2p.listen.udp={P2P_PORT}"),
        "--p2p.scoring.peers=none".to_string(),
        "--p2p.ban.peers=false".to_string(),
        "--rpc.enable-admin".to_string(),
        "--rpc.addr=0.0.0.0".to_string(),
        format!("--rpc.port={RPC_PORT}"),
    ]
}

fn follower_args(config: &OpNodeFollowerConfig) -> Vec<String> {
    let static_peer = format!(
        "/dns4/{}/tcp/{}/p2p/{}",
        config.builder_op_node_name, L2_OP_NODE_P2P_PORT, config.builder_op_node_peer_id
    );

    vec![
        format!("--rollup.config={ROLLUP_CONFIG_PATH}"),
        format!("--rollup.l1-chain-config={L1_GENESIS_PATH}"),
        format!("--l1={}", config.l1_rpc_url),
        format!("--l1.beacon={}", config.l1_beacon_url),
        format!("--l2={}", config.l2_engine_url),
        format!("--l2.jwt-secret={JWT_PATH}"),
        format!("--p2p.static={static_peer}"),
        "--p2p.scoring.peers=none".to_string(),
        "--p2p.ban.peers=false".to_string(),
        "--rpc.addr=0.0.0.0".to_string(),
        format!("--rpc.port={RPC_PORT}"),
    ]
}
