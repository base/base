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
//! To enable the build-then-reconcile behavior, the shadow runs with
//! `shadow_blocks_per_cycle` set: it buffers the active sequencer's gossiped
//! canonical payloads, builds that many private blocks per cycle, then reorgs
//! back to the canonical chain (forgetting its private blocks) before starting
//! the next cycle. Accepting the canonical gossip requires the shadow's
//! `unsafe_block_signer` to match the active sequencer's address.

use std::{num::NonZeroU64, time::Duration};

use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::JwtSecret;
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
    /// Distinct sequencer signing key. Must differ from the active sequencer key
    /// so that blocks built by this shadow are rejected as non-canonical.
    pub sequencer_key: B256,
    /// Address of the active sequencer. Used as this shadow's `unsafe_block_signer`
    /// so the shadow accepts the active sequencer's gossiped canonical payloads
    /// (a prerequisite for reconciliation).
    pub active_sequencer_address: Address,
    /// Number of private blocks the shadow builds per reconciliation cycle.
    pub shadow_blocks_per_cycle: NonZeroU64,
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
    /// L1 slot duration in seconds, used as the consensus derivation poll override.
    pub l1_slot_duration: u64,
}

/// A running shadow sequencer: an isolated builder execution layer plus a
/// consensus node running in `NodeMode::Sequencer`.
#[derive(Debug)]
pub struct ShadowSequencer {
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
    /// The consensus node signs its own gossiped blocks with `sequencer_key` (a
    /// key the rest of the network does not trust, so its blocks are rejected as
    /// non-canonical), but sets `unsafe_block_signer` to the active sequencer's
    /// address so it accepts the canonical payloads gossiped by the active
    /// sequencer. Those buffered canonical payloads drive reconciliation.
    pub async fn start(config: ShadowSequencerConfig) -> Result<Self> {
        let chain_spec = InProcessBuilderConfig::chain_spec_from_genesis_json(&config.l2_genesis)
            .wrap_err("Failed to parse shadow builder L2 chain spec")?;
        let builder = InProcessBuilder::start(InProcessBuilderConfig {
            chain_spec,
            datadir: None,
            jwt_secret: config.jwt_secret,
            http_port: None,
            ws_port: None,
            auth_port: None,
            p2p_port: None,
            flashblocks_port: None,
            metrics_port: None,
            enable_experimental_validity_transactions: false,
            payload_builder_cutover: false,
            extra_extensions: Vec::new(),
            block_time: Duration::from_secs(config.rollup_config.block_time),
            persistence_threshold: None,
            txpool_max_transactions: None,
            txpool_max_size_mb: None,
            txpool_max_account_slots: None,
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
            unsafe_block_signer: config.active_sequencer_address,
            l1_slot_duration_override: Some(config.l1_slot_duration),
            sequencer_stopped: true,
            verifier_l1_confs: 0,
            shadow_blocks_per_cycle: Some(config.shadow_blocks_per_cycle),
            upgrade_signal: None,
        })
        .await
        .wrap_err("Failed to start shadow consensus")?;

        consensus
            .connect_peer(&config.active_consensus_p2p_addr)
            .await
            .wrap_err("Failed to connect shadow consensus to active sequencer")?;

        consensus.start_sequencer().await.wrap_err("Failed to start shadow sequencer")?;

        Ok(Self { builder, consensus })
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

    /// Stops the shadow consensus task and gracefully shuts down its execution node.
    pub async fn shutdown(self) -> Result<()> {
        let Self { builder, consensus } = self;
        drop(consensus);
        builder.shutdown().await.wrap_err("Failed to shut down shadow builder")
    }
}
