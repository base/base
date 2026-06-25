//! L2 stack orchestration (Builder + Consensus + Batcher).
//!
//! This module provides [`L2Stack`], which composes a complete L2 network by orchestrating:
//! - Builder execution layer (in-process, produces blocks and sequences transactions)
//! - Consensus layer (in-process, derives L2 blocks from L1 data)
//! - Batcher (in-process, submits L2 transaction batches to L1)
//! - Client execution layer (in-process, follows the L2 and builds pending state using Flashblocks)

use std::time::Duration;

use alloy_genesis::ChainConfig;
use alloy_primitives::B256;
use alloy_rpc_types_engine::JwtSecret;
use base_common_genesis::RollupConfig;
use base_consensus_node::NodeMode;
use base_tx_forwarding::TxForwardingConfig;
use eyre::{Result, WrapErr};
use url::Url;

use super::{
    InProcessBatcher, InProcessBatcherConfig, InProcessBuilder, InProcessBuilderConfig,
    InProcessClient, InProcessClientConfig, InProcessConsensus, InProcessConsensusConfig,
    InProcessFollowConsensus, InProcessFollowConsensusConfig, L2ContainerConfig,
};
use crate::config::SEQUENCER;

/// Consensus mode used by the L2 client node.
#[derive(Debug, Clone, Copy, Default)]
pub enum L2ClientConsensusMode {
    /// Run the client consensus node as a normal validator.
    #[default]
    Validator,
    /// Run the client consensus node in follow mode against the builder RPC.
    Follow,
}

/// Configuration for the L2 stack.
#[derive(Debug, Clone)]
pub struct L2StackConfig {
    /// L2 genesis JSON content.
    pub l2_genesis: Vec<u8>,
    /// Rollup configuration JSON.
    pub rollup_config: Vec<u8>,
    /// L1 genesis JSON (for consensus chain spec).
    pub l1_genesis: Vec<u8>,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// P2P private key for consensus node identity.
    pub p2p_key: B256,
    /// Sequencer private key for block signing.
    pub sequencer_key: B256,
    /// Batcher private key (hex-encoded string, e.g., "0x...").
    pub batcher_key: B256,
    /// L1 RPC endpoint URL (host-accessible).
    pub l1_rpc_url: String,
    /// L1 beacon API endpoint URL (host-accessible).
    pub l1_beacon_url: String,
    /// Optional container configuration for stable naming and port binding.
    pub container_config: Option<L2ContainerConfig>,
    /// Optional transaction forwarding configuration for the client node.
    /// When set, the client will forward transactions to builder RPC endpoints.
    pub tx_forwarding_config: Option<TxForwardingConfig>,
    /// Number of L1 blocks to keep distance from the L1 head for the client (validator)
    /// consensus node's derivation pipeline.
    pub verifier_l1_confs: u64,
    /// Consensus mode for the L2 client node.
    pub client_consensus_mode: L2ClientConsensusMode,
}

/// Running L2 client consensus node.
#[derive(Debug)]
pub enum L2ClientConsensus {
    /// Standard validator consensus node.
    Validator(InProcessConsensus),
    /// Follow-mode consensus node.
    Follow(InProcessFollowConsensus),
}

impl L2ClientConsensus {
    /// Returns the RPC URL for this consensus node.
    pub fn rpc_url(&self) -> Url {
        match self {
            Self::Validator(consensus) => consensus.rpc_url(),
            Self::Follow(consensus) => consensus.rpc_url(),
        }
    }
}

/// A complete L2 network stack composed of Builder + Consensus + Batcher.
///
/// This struct orchestrates the full L2 infrastructure:
/// - Builder execution layer (in-process, produces blocks and sequences transactions)
/// - Consensus layer (in-process, derives L2 blocks from L1 data)
/// - Batcher (in-process, submits L2 transaction batches to L1)
///
/// The startup order is:
/// 1. Builder starts first (in-process EL)
/// 2. Builder consensus node connects to builder's engine API (in-process CL, Sequencer mode)
/// 3. Batcher connects to builder RPC and builder consensus RPC
/// 4. Client starts (in-process EL)
/// 5. Client consensus node connects to client's engine API
/// 6. Validator-mode client consensus connects to builder consensus via P2P
pub struct L2Stack {
    builder: InProcessBuilder,
    builder_consensus: InProcessConsensus,
    batcher: InProcessBatcher,
    client: InProcessClient,
    client_consensus: L2ClientConsensus,
}

impl std::fmt::Debug for L2Stack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L2Stack")
            .field("builder", &self.builder)
            .field("builder_consensus", &self.builder_consensus)
            .field("batcher", &self.batcher)
            .field("client", &self.client)
            .field("client_consensus", &self.client_consensus)
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

        let l1_rpc_url: Url = config.l1_rpc_url.parse().wrap_err("Invalid L1 RPC URL")?;
        let l1_beacon_url: Url = config.l1_beacon_url.parse().wrap_err("Invalid L1 beacon URL")?;

        let rollup_config: RollupConfig = serde_json::from_slice(&config.rollup_config)
            .wrap_err("Failed to parse rollup config")?;
        let l1_chain_config: ChainConfig = serde_json::from_slice(&config.l1_genesis)
            .wrap_err("Failed to parse L1 chain config")?;

        // 1. Start the builder (in-process EL).
        let builder_config = InProcessBuilderConfig {
            genesis_json: config.l2_genesis.clone(),
            jwt_secret: config.jwt_secret,
            http_port: container_config.and_then(|c| c.builder_http_port),
            ws_port: container_config.and_then(|c| c.builder_ws_port),
            auth_port: container_config.and_then(|c| c.builder_auth_port),
            p2p_port: container_config.and_then(|c| c.builder_p2p_port),
            flashblocks_port: container_config.and_then(|c| c.builder_flashblocks_port),
        };
        let builder = InProcessBuilder::start(builder_config)
            .await
            .wrap_err("Failed to start in-process builder")?;

        // 2. Start builder consensus (in-process CL, Sequencer mode).
        //    The sequencer starts in stopped mode so that blocks are not produced until the
        //    validator is connected via P2P — otherwise the first blocks would be lost via gossip
        //    and the validator's EL would be unable to validate later blocks (missing parent).
        let builder_consensus_config = InProcessConsensusConfig {
            rollup_config: rollup_config.clone(),
            l1_chain_config: l1_chain_config.clone(),
            jwt_secret: config.jwt_secret,
            l1_rpc_url: l1_rpc_url.clone(),
            l1_beacon_url: l1_beacon_url.clone(),
            l2_engine_url: builder.engine_url()?,
            mode: NodeMode::Sequencer,
            sequencer_key: Some(config.sequencer_key),
            p2p_key: Some(config.p2p_key),
            rpc_port: container_config.and_then(|c| c.builder_consensus_rpc_port),
            p2p_tcp_port: container_config.and_then(|c| c.builder_consensus_p2p_tcp_port),
            p2p_udp_port: container_config.and_then(|c| c.builder_consensus_p2p_udp_port),
            unsafe_block_signer: SEQUENCER.address,
            l1_slot_duration_override: Some(4),
            sequencer_stopped: true,
            verifier_l1_confs: 0,
        };
        let builder_consensus = InProcessConsensus::start(builder_consensus_config)
            .await
            .wrap_err("Failed to start builder consensus")?;

        // 3. Start the in-process batcher, pointing at builder consensus RPC.
        // No host gateway translation needed — the batcher runs in the same process as the test.
        let batcher = InProcessBatcher::start(InProcessBatcherConfig {
            l1_rpc_url: l1_rpc_url.clone(),
            l2_rpc_url: builder.rpc_url()?,
            rollup_rpc_url: builder_consensus.rpc_url(),
            batcher_key: config.batcher_key,
        })
        .await
        .wrap_err("Failed to start in-process batcher")?;

        // 4. Start the client (in-process EL).
        // If tx forwarding is enabled, configure it with the builder's RPC URL
        let tx_forwarding_config = if let Some(mut cfg) = config.tx_forwarding_config {
            // Add the builder's RPC URL to the forwarding config
            // The config may have empty builder_urls which we need to populate
            if cfg.builder_urls.is_empty() {
                cfg.builder_urls = vec![builder.rpc_url()?];
            }
            Some(cfg)
        } else {
            None
        };

        let client_config = InProcessClientConfig {
            genesis_json: config.l2_genesis.clone(),
            jwt_secret: config.jwt_secret,
            builder_rpc_url: builder.rpc_url()?.to_string(),
            builder_flashblocks_url: builder.flashblocks_url(),
            builder_p2p_enode: builder.p2p_enode(),
            http_port: container_config.and_then(|c| c.client_http_port),
            ws_port: container_config.and_then(|c| c.client_ws_port),
            auth_port: container_config.and_then(|c| c.client_auth_port),
            p2p_port: container_config.and_then(|c| c.client_p2p_port),
            tx_forwarding_config,
        };
        let client = InProcessClient::start(client_config)
            .await
            .wrap_err("Failed to start in-process client")?;

        // 5. Start client consensus.
        let client_consensus = match config.client_consensus_mode {
            L2ClientConsensusMode::Validator => {
                let client_consensus_config = InProcessConsensusConfig {
                    rollup_config,
                    l1_chain_config,
                    jwt_secret: config.jwt_secret,
                    l1_rpc_url,
                    l1_beacon_url,
                    l2_engine_url: client.engine_url()?,
                    mode: NodeMode::Validator,
                    sequencer_key: None,
                    p2p_key: None,
                    rpc_port: container_config.and_then(|c| c.client_consensus_rpc_port),
                    p2p_tcp_port: container_config.and_then(|c| c.client_consensus_p2p_tcp_port),
                    p2p_udp_port: container_config.and_then(|c| c.client_consensus_p2p_udp_port),
                    unsafe_block_signer: SEQUENCER.address,
                    l1_slot_duration_override: Some(4),
                    sequencer_stopped: false,
                    verifier_l1_confs: config.verifier_l1_confs,
                };
                let client_consensus = InProcessConsensus::start(client_consensus_config)
                    .await
                    .wrap_err("Failed to start client consensus")?;

                // Connect the client consensus to the builder consensus via P2P.
                let builder_p2p_addr = builder_consensus.p2p_addr();
                client_consensus
                    .connect_peer(&builder_p2p_addr)
                    .await
                    .wrap_err("Failed to connect client consensus to builder consensus")?;
                L2ClientConsensus::Validator(client_consensus)
            }
            L2ClientConsensusMode::Follow => {
                // Follow-mode consensus polls the builder RPC directly, so it does not need a P2P
                // peer connection before the sequencer starts producing blocks.
                let client_consensus_config = InProcessFollowConsensusConfig {
                    rollup_config,
                    jwt_secret: config.jwt_secret,
                    l1_rpc_url,
                    local_l2_rpc_url: client.rpc_url()?,
                    source_l2_rpc_url: builder.rpc_url()?,
                    l2_engine_url: client.engine_url()?,
                    rpc_port: container_config.and_then(|c| c.client_consensus_rpc_port),
                    insert_delay: Duration::ZERO,
                };
                let client_consensus = InProcessFollowConsensus::start(client_consensus_config)
                    .await
                    .wrap_err("Failed to start follow client consensus")?;
                L2ClientConsensus::Follow(client_consensus)
            }
        };

        // 6. Start the sequencer after the client consensus is ready.
        builder_consensus
            .start_sequencer()
            .await
            .wrap_err("Failed to start sequencer after peer connection")?;

        Ok(Self { builder, builder_consensus, batcher, client, client_consensus })
    }

    /// Returns a reference to the in-process builder.
    pub const fn builder(&self) -> &InProcessBuilder {
        &self.builder
    }

    /// Returns a reference to the builder's consensus node.
    pub const fn builder_consensus(&self) -> &InProcessConsensus {
        &self.builder_consensus
    }

    /// Returns a reference to the in-process batcher.
    pub const fn batcher(&self) -> &InProcessBatcher {
        &self.batcher
    }

    /// Returns a reference to the in-process client.
    pub const fn client(&self) -> &InProcessClient {
        &self.client
    }

    /// Returns a reference to the client's consensus node.
    pub const fn client_consensus(&self) -> &L2ClientConsensus {
        &self.client_consensus
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

    /// Returns the builder consensus node's RPC URL.
    pub fn builder_consensus_rpc_url(&self) -> Url {
        self.builder_consensus.rpc_url()
    }

    /// Returns the client consensus node's RPC URL.
    pub fn client_consensus_rpc_url(&self) -> Url {
        self.client_consensus.rpc_url()
    }
}
