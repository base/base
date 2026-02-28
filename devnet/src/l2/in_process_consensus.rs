//! In-process consensus node for L2 devnet.
//!
//! Runs `base-consensus-node` directly in the test process, eliminating the Docker
//! dependency for the consensus layer. Mirrors the pattern used by
//! [`InProcessBuilder`](super::InProcessBuilder) and [`InProcessClient`](super::InProcessClient).

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use alloy_primitives::B256;
use alloy_rpc_types_engine::JwtSecret;
use alloy_signer_local::PrivateKeySigner;
use base_builder_core::test_utils::get_available_port;
use base_consensus_disc::LocalNode;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_consensus_node::{
    EngineConfig, L1ConfigBuilder, NetworkConfig, NodeMode, RollupNodeBuilder, SequencerConfig,
};
use base_consensus_peers::{PeerScoreLevel, SecretKeyLoader};
use base_consensus_rpc::{OpP2PApiClient, RpcBuilder};
use base_consensus_sources::BlockSigner;
use eyre::{Result, WrapErr};
use jsonrpsee::http_client::HttpClientBuilder;
use tokio::task::JoinHandle;
use tracing::{error, info};
use url::Url;

/// Configuration for starting an in-process consensus node.
pub struct InProcessConsensusConfig {
    /// Parsed rollup configuration.
    pub rollup_config: RollupConfig,
    /// Parsed L1 chain configuration.
    pub l1_chain_config: L1ChainConfig,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// L1 RPC endpoint URL.
    pub l1_rpc_url: Url,
    /// L1 beacon API endpoint URL.
    pub l1_beacon_url: Url,
    /// L2 engine API URL (builder or client).
    pub l2_engine_url: Url,
    /// Node mode (Sequencer or Validator).
    pub mode: NodeMode,
    /// Sequencer signing key (required for Sequencer mode).
    pub sequencer_key: Option<B256>,
    /// P2P identity key.
    pub p2p_key: Option<B256>,
    /// Optional fixed RPC port.
    pub rpc_port: Option<u16>,
    /// Optional fixed P2P TCP port.
    pub p2p_tcp_port: Option<u16>,
    /// Optional fixed P2P UDP port.
    pub p2p_udp_port: Option<u16>,
    /// Unsafe block signer address.
    pub unsafe_block_signer: alloy_primitives::Address,
    /// L1 slot duration override in seconds.
    pub l1_slot_duration_override: Option<u64>,
}

/// A running in-process consensus node.
pub struct InProcessConsensus {
    rpc_addr: SocketAddr,
    p2p_tcp_port: u16,
    peer_id: String,
    _handle: JoinHandle<()>,
}

impl std::fmt::Debug for InProcessConsensus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessConsensus")
            .field("rpc_addr", &self.rpc_addr)
            .field("p2p_tcp_port", &self.p2p_tcp_port)
            .field("peer_id", &self.peer_id)
            .finish()
    }
}

impl InProcessConsensus {
    /// Starts an in-process consensus node with the given configuration.
    pub async fn start(config: InProcessConsensusConfig) -> Result<Self> {
        let rollup_config = config.rollup_config;
        let l1_chain_config = config.l1_chain_config;

        let rpc_port = config.rpc_port.unwrap_or_else(get_available_port);
        let p2p_tcp_port = config.p2p_tcp_port.unwrap_or_else(get_available_port);
        let p2p_udp_port = config.p2p_udp_port.unwrap_or_else(get_available_port);
        let listen_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);

        // Build keypair from P2P key or generate a random one.
        let keypair = if let Some(mut p2p_key) = config.p2p_key {
            SecretKeyLoader::parse(&mut p2p_key.0).wrap_err("Failed to parse P2P key")?
        } else {
            libp2p::identity::Keypair::generate_secp256k1()
        };

        let peer_id = keypair.public().to_peer_id().to_string();

        // Build the signing key for discv5 from the same P2P key material.
        let signing_key = extract_signing_key(&keypair)?;

        let local_node = LocalNode::new(signing_key, listen_ip, p2p_tcp_port, p2p_udp_port);

        let gossip_address: libp2p::Multiaddr = format!("/ip4/{listen_ip}/tcp/{p2p_tcp_port}")
            .parse()
            .wrap_err("Failed to parse gossip multiaddr")?;

        let mut net_config = NetworkConfig::new(
            rollup_config.clone(),
            local_node,
            gossip_address,
            config.unsafe_block_signer,
        );
        net_config.scoring = PeerScoreLevel::Off;
        net_config.keypair = keypair;

        // For sequencer mode, attach a gossip signer.
        if config.mode == NodeMode::Sequencer
            && let Some(seq_key) = config.sequencer_key
        {
            let signer = PrivateKeySigner::from_bytes(&seq_key)
                .wrap_err("Failed to create sequencer signer")?;
            net_config.gossip_signer = Some(BlockSigner::Local(signer));
        }

        let l1_config = L1ConfigBuilder {
            chain_config: l1_chain_config,
            trust_rpc: true,
            beacon: config.l1_beacon_url,
            rpc_url: config.l1_rpc_url.clone(),
            slot_duration_override: config.l1_slot_duration_override,
        };

        let engine_config = EngineConfig {
            config: Arc::new(rollup_config.clone()),
            l2_url: config.l2_engine_url,
            l2_jwt_secret: config.jwt_secret,
            l1_url: config.l1_rpc_url,
            mode: config.mode,
        };

        let rpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port);
        let rpc_config = RpcBuilder {
            no_restart: true,
            socket: rpc_addr,
            enable_admin: true,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
        };

        let mut builder = RollupNodeBuilder::new(
            rollup_config,
            l1_config,
            true,
            engine_config,
            net_config,
            Some(rpc_config),
        );

        if config.mode == NodeMode::Sequencer {
            builder = builder.with_sequencer_config(SequencerConfig {
                sequencer_stopped: false,
                sequencer_recovery_mode: false,
                conductor_rpc_url: None,
                l1_conf_delay: 0,
            });
        }

        let node = builder.build();

        let mode = config.mode;
        let handle = tokio::spawn(async move {
            info!(mode = %mode, rpc_port, "starting in-process consensus node");
            if let Err(e) = node.start().await {
                error!(error = %e, "in-process consensus node failed");
            }
        });

        // Wait for the RPC server to become available.
        wait_for_rpc(rpc_addr).await?;

        Ok(Self { rpc_addr, p2p_tcp_port, peer_id, _handle: handle })
    }

    /// Connects this node to a peer at the given libp2p multiaddr via the `opp2p_connectPeer` RPC.
    pub async fn connect_peer(&self, multiaddr: &str) -> Result<()> {
        let client = HttpClientBuilder::default()
            .build(self.rpc_url().as_str())
            .wrap_err("Failed to build RPC client")?;

        client
            .opp2p_connect_peer(multiaddr.to_string())
            .await
            .wrap_err("Failed to connect peer via RPC")
    }

    /// Returns the RPC URL for this consensus node.
    pub fn rpc_url(&self) -> Url {
        Url::parse(&format!("http://{}:{}", self.rpc_addr.ip(), self.rpc_addr.port()))
            .expect("valid RPC URL")
    }

    /// Returns the P2P multiaddr including the peer ID.
    pub fn p2p_addr(&self) -> String {
        format!("/ip4/127.0.0.1/tcp/{}/p2p/{}", self.p2p_tcp_port, self.peer_id)
    }

    /// Returns the RPC port.
    pub const fn rpc_port(&self) -> u16 {
        self.rpc_addr.port()
    }

    /// Returns the P2P TCP port.
    pub const fn p2p_tcp_port(&self) -> u16 {
        self.p2p_tcp_port
    }

    /// Returns the libp2p peer ID string.
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

impl Drop for InProcessConsensus {
    fn drop(&mut self) {
        self._handle.abort();
    }
}

/// Extracts the `k256::ecdsa::SigningKey` from a libp2p [`Keypair`](libp2p::identity::Keypair).
fn extract_signing_key(keypair: &libp2p::identity::Keypair) -> Result<k256::ecdsa::SigningKey> {
    let secp_keypair = keypair
        .clone()
        .try_into_secp256k1()
        .map_err(|_| eyre::eyre!("Keypair is not secp256k1"))?;
    let secret_bytes = secp_keypair.secret().to_bytes();
    k256::ecdsa::SigningKey::from_bytes((&secret_bytes).into())
        .wrap_err("Failed to create signing key from keypair bytes")
}

/// Polls the RPC endpoint until it responds or times out.
async fn wait_for_rpc(addr: SocketAddr) -> Result<()> {
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let client = reqwest::Client::new();

    for i in 0..60 {
        match client.get(&url).send().await {
            Ok(_) => {
                info!(attempts = i + 1, "consensus RPC is ready");
                return Ok(());
            }
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }

    Err(eyre::eyre!("Consensus RPC at {url} did not become ready within 30s"))
}
