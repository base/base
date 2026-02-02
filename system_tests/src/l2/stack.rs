//! L2 stack orchestration (Builder + op-node + Batcher).
//!
//! This module provides [`L2Stack`], which composes a complete L2 network by orchestrating:
//! - Builder execution layer (in-process, produces blocks and sequences transactions)
//! - op-node consensus layer (Docker container, derives L2 blocks from L1 data)
//! - Batcher (Docker container, submits L2 transaction batches to L1)
//!
//! Optionally, an in-process client can be added for follower testing.

use eyre::{Result, WrapErr};
use url::Url;

use super::{
    BatcherConfig, BatcherContainer, InProcessBuilder, InProcessBuilderConfig, InProcessClient,
    InProcessClientConfig, L2ContainerConfig, OpNodeConfig, OpNodeContainer, OpNodeFollowerConfig,
    OpNodeFollowerContainer,
};
use crate::setup::BUILDER_LIBP2P_PEER_ID;

/// Configuration for the L2 stack.
#[derive(Debug, Clone)]
pub struct L2StackConfig {
    /// L2 genesis JSON content.
    pub l2_genesis: Vec<u8>,
    /// Rollup configuration JSON.
    pub rollup_config: Vec<u8>,
    /// L1 genesis JSON (for op-node chain spec).
    pub l1_genesis: Vec<u8>,
    /// JWT secret hex for Engine API authentication.
    pub jwt_secret_hex: Vec<u8>,
    /// P2P private key (hex-encoded, no 0x prefix) for op-node libp2p identity.
    pub p2p_key: Vec<u8>,
    /// Sequencer private key (hex-encoded with 0x prefix) for block signing.
    pub sequencer_key: Vec<u8>,
    /// Batcher private key (hex-encoded string, e.g., "0x...").
    pub batcher_key: String,
    /// L1 RPC endpoint URL.
    pub l1_rpc_url: String,
    /// L1 beacon API endpoint URL.
    pub l1_beacon_url: String,
    /// Optional container configuration for stable naming and port binding.
    pub container_config: Option<L2ContainerConfig>,
}

/// A complete L2 network stack composed of Builder + op-node + Batcher.
///
/// This struct orchestrates the full L2 infrastructure:
/// - Builder execution layer (in-process, produces blocks and sequences transactions)
/// - op-node consensus layer (Docker container, derives L2 blocks from L1 data)
/// - Batcher (Docker container, submits L2 transaction batches to L1)
///
/// The startup order is:
/// 1. Builder starts first (in-process EL)
/// 2. op-node connects to builder's engine API via host.docker.internal (CL)
/// 3. Batcher connects to builder RPC via host.docker.internal and op-node RPC
///
/// # Example
///
/// ```ignore
/// use system_tests::l2::{L2Stack, L2StackConfig};
///
/// let config = L2StackConfig {
///     l2_genesis: genesis_json,
///     rollup_config: rollup_json,
///     jwt_secret_hex: jwt_secret,
///     sequencer_key: seq_key,
///     batcher_key: batch_key,
///     l1_rpc_url: "http://localhost:8545".to_string(),
///     l1_beacon_url: "http://localhost:5052".to_string(),
/// };
///
/// let stack = L2Stack::start(config).await?;
/// let rpc_url = stack.rpc_url()?;
/// println!("L2 RPC available at: {}", rpc_url);
/// ```
pub struct L2Stack {
    builder: InProcessBuilder,
    op_node: OpNodeContainer,
    batcher: BatcherContainer,
    client: InProcessClient,
    #[allow(dead_code)]
    client_op_node: OpNodeFollowerContainer,
}

impl std::fmt::Debug for L2Stack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L2Stack")
            .field("builder", &self.builder)
            .field("op_node", &self.op_node)
            .field("batcher", &self.batcher)
            .field("client", &self.client)
            .field("client_op_node", &"OpNodeFollowerContainer")
            .finish()
    }
}

impl L2Stack {
    /// Starts a complete L2 network stack with builder, client, and all supporting services.
    ///
    /// # Errors
    ///
    /// Returns an error if any component fails to start.
    pub async fn start(config: L2StackConfig) -> Result<Self> {
        let container_config = config.container_config.as_ref();
        let builder_config = InProcessBuilderConfig {
            genesis_json: config.l2_genesis.clone(),
            jwt_secret_hex: config.jwt_secret_hex.clone(),
            http_port: container_config.and_then(|c| c.builder_http_port),
            ws_port: container_config.and_then(|c| c.builder_ws_port),
            auth_port: container_config.and_then(|c| c.builder_auth_port),
            p2p_port: container_config.and_then(|c| c.builder_p2p_port),
            flashblocks_port: container_config.and_then(|c| c.builder_flashblocks_port),
        };
        let builder = InProcessBuilder::start(builder_config)
            .await
            .wrap_err("Failed to start in-process builder")?;

        let op_node_config = OpNodeConfig {
            rollup_config: config.rollup_config.clone(),
            l1_genesis: config.l1_genesis.clone(),
            jwt_secret_hex: config.jwt_secret_hex.clone(),
            p2p_key: config.p2p_key,
            sequencer_key: config.sequencer_key,
            l1_rpc_url: config.l1_rpc_url.clone(),
            l1_beacon_url: config.l1_beacon_url.clone(),
            l2_engine_url: builder.host_engine_url(),
            l2_engine_port: builder.engine_port(),
        };
        let op_node = OpNodeContainer::start(op_node_config, config.container_config.as_ref())
            .await
            .wrap_err("Failed to start op-node")?;

        let batcher_config = BatcherConfig {
            l1_rpc_url: config.l1_rpc_url.clone(),
            l2_rpc_url: builder.host_rpc_url(),
            l2_rpc_port: builder.rpc_port(),
            rollup_rpc_url: op_node.internal_rpc_url(),
            batcher_key: config.batcher_key,
        };
        let batcher = BatcherContainer::start(batcher_config, config.container_config.as_ref())
            .await
            .wrap_err("Failed to start batcher")?;

        let client_config = InProcessClientConfig {
            genesis_json: config.l2_genesis.clone(),
            jwt_secret_hex: config.jwt_secret_hex.clone(),
            builder_rpc_url: builder.rpc_url()?.to_string(),
            builder_flashblocks_url: builder.flashblocks_url(),
            builder_p2p_enode: builder.p2p_enode(),
            http_port: container_config.and_then(|c| c.client_http_port),
            ws_port: container_config.and_then(|c| c.client_ws_port),
            auth_port: container_config.and_then(|c| c.client_auth_port),
            p2p_port: container_config.and_then(|c| c.client_p2p_port),
        };
        let client = InProcessClient::start(client_config)
            .await
            .wrap_err("Failed to start in-process client")?;

        let client_op_node_config = OpNodeFollowerConfig {
            rollup_config: config.rollup_config,
            l1_genesis: config.l1_genesis,
            jwt_secret_hex: config.jwt_secret_hex,
            l1_rpc_url: config.l1_rpc_url,
            l1_beacon_url: config.l1_beacon_url,
            l2_engine_url: client.host_engine_url(),
            l2_engine_port: client.engine_port(),
            builder_op_node_name: op_node.name().to_string(),
            builder_op_node_peer_id: BUILDER_LIBP2P_PEER_ID.to_string(),
        };
        let client_op_node =
            OpNodeFollowerContainer::start(client_op_node_config, config.container_config.as_ref())
                .await
                .wrap_err("Failed to start client op-node follower")?;

        Ok(Self { builder, op_node, batcher, client, client_op_node })
    }

    /// Returns a reference to the in-process builder.
    pub const fn builder(&self) -> &InProcessBuilder {
        &self.builder
    }

    /// Returns a reference to the op-node container.
    pub const fn op_node(&self) -> &OpNodeContainer {
        &self.op_node
    }

    /// Returns a reference to the batcher container.
    pub const fn batcher(&self) -> &BatcherContainer {
        &self.batcher
    }

    /// Returns a reference to the in-process client.
    pub const fn client(&self) -> &InProcessClient {
        &self.client
    }

    /// Returns the builder's HTTP RPC URL.
    pub fn rpc_url(&self) -> Result<Url> {
        self.builder.rpc_url()
    }

    /// Returns the builder's WebSocket URL.
    pub fn ws_url(&self) -> Result<Url> {
        self.builder.ws_url()
    }

    /// Returns the client's HTTP RPC URL.
    pub fn client_rpc_url(&self) -> Result<Url> {
        self.client.rpc_url()
    }

    /// Returns the op-node's RPC URL.
    pub async fn op_node_rpc_url(&self) -> Result<Url> {
        self.op_node.rpc_url().await
    }
}
