//! Shadow sequencer node for L2 system test stacks.
//!
//! A shadow sequencer is a full `NodeMode::Sequencer` node (its own builder
//! execution layer plus an in-process consensus node) that runs in parallel to
//! the active sequencer. It builds and seals real blocks from its own mempool,
//! but signs them with a distinct key so the rest of the network rejects them as
//! non-canonical. This mirrors the "Shadow" role in the Shadow Block Builder
//! design: build candidate blocks to validate new code without ever advancing
//! the canonical chain tip observed by other nodes.
//!
//! The build-but-forget reorg behavior (a shadow re-orging itself back to the
//! active sequencer's canonical state) is intentionally NOT implemented here; it
//! is the subject of a follow-up change. Tests may therefore assert the desired
//! end state and observe it fail against the current node implementation.

use alloy_genesis::ChainConfig;
use alloy_primitives::B256;
use alloy_rpc_types_engine::JwtSecret;
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::RollupConfig;
use base_consensus_node::NodeMode;
use eyre::{Result, WrapErr};
use url::Url;

use super::{
    InProcessBuilder, InProcessBuilderConfig, InProcessConsensus, InProcessConsensusConfig,
};

/// Configuration for starting a single [`ShadowSequencer`].
#[derive(Debug, Clone)]
pub struct ShadowSequencerConfig {
    /// Zero-based index of this shadow sequencer, used only for logging/identity.
    pub index: usize,
    /// Distinct sequencer signing key. Must differ from the active sequencer key
    /// so that blocks built by this shadow are rejected as non-canonical.
    pub sequencer_key: B256,
    /// L2 genesis JSON content (shared with the active stack).
    pub l2_genesis: Vec<u8>,
    /// Parsed rollup configuration (shared with the active stack).
    pub rollup_config: RollupConfig,
    /// Parsed L1 chain configuration (shared with the active stack).
    pub l1_chain_config: ChainConfig,
    /// JWT secret for Engine API authentication.
    pub jwt_secret: JwtSecret,
    /// L1 RPC endpoint URL.
    pub l1_rpc_url: Url,
    /// L1 beacon API endpoint URL.
    pub l1_beacon_url: Url,
    /// P2P multiaddr of the active sequencer's consensus node to peer with.
    pub active_consensus_p2p_addr: String,
}

/// A running shadow sequencer: an isolated builder execution layer plus a
/// consensus node running in `NodeMode::Sequencer`.
#[derive(Debug)]
pub struct ShadowSequencer {
    index: usize,
    sequencer_key: B256,
    builder: InProcessBuilder,
    consensus: InProcessConsensus,
}

impl ShadowSequencer {
    /// Starts a shadow sequencer and activates block production.
    ///
    /// Startup order mirrors the active sequencer path: the builder EL starts
    /// first, then the consensus node (Sequencer mode, started stopped), which is
    /// peered to the active sequencer before block production is enabled.
    ///
    /// The consensus node uses its own signing address as `unsafe_block_signer`,
    /// so it does not accept the active sequencer's gossiped blocks and instead
    /// builds an independent chain from its own mempool. Its own blocks are in
    /// turn rejected by the active network because they are signed with a key the
    /// rest of the network does not trust.
    pub async fn start(config: ShadowSequencerConfig) -> Result<Self> {
        let unsafe_block_signer = PrivateKeySigner::from_bytes(&config.sequencer_key)
            .wrap_err("Failed to derive shadow sequencer address")?
            .address();

        let builder = InProcessBuilder::start(InProcessBuilderConfig {
            genesis_json: config.l2_genesis,
            jwt_secret: config.jwt_secret,
            http_port: None,
            ws_port: None,
            auth_port: None,
            p2p_port: None,
            flashblocks_port: None,
        })
        .await
        .wrap_err("Failed to start shadow builder")?;

        let consensus = InProcessConsensus::start(InProcessConsensusConfig {
            rollup_config: config.rollup_config,
            l1_chain_config: config.l1_chain_config,
            jwt_secret: config.jwt_secret,
            l1_rpc_url: config.l1_rpc_url,
            l1_beacon_url: config.l1_beacon_url,
            l2_engine_url: builder.engine_url()?,
            mode: NodeMode::Sequencer,
            sequencer_key: Some(config.sequencer_key),
            p2p_key: None,
            rpc_port: None,
            p2p_tcp_port: None,
            p2p_udp_port: None,
            unsafe_block_signer,
            l1_slot_duration_override: Some(4),
            sequencer_stopped: true,
            verifier_l1_confs: 0,
        })
        .await
        .wrap_err("Failed to start shadow consensus")?;

        consensus
            .connect_peer(&config.active_consensus_p2p_addr)
            .await
            .wrap_err("Failed to connect shadow consensus to active sequencer")?;

        consensus.start_sequencer().await.wrap_err("Failed to start shadow sequencer")?;

        Ok(Self { index: config.index, sequencer_key: config.sequencer_key, builder, consensus })
    }

    /// Returns the zero-based index of this shadow sequencer.
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Returns the signing key used by this shadow sequencer.
    pub const fn sequencer_key(&self) -> B256 {
        self.sequencer_key
    }

    /// Returns a reference to the shadow's builder execution layer.
    pub const fn builder(&self) -> &InProcessBuilder {
        &self.builder
    }

    /// Returns a reference to the shadow's consensus node.
    pub const fn consensus(&self) -> &InProcessConsensus {
        &self.consensus
    }

    /// Returns the shadow builder's HTTP RPC URL.
    pub fn rpc_url(&self) -> Result<Url> {
        self.builder.rpc_url()
    }

    /// Returns the shadow consensus node's RPC URL.
    pub fn consensus_rpc_url(&self) -> Url {
        self.consensus.rpc_url()
    }
}
