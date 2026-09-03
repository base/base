//! Fresh builder and sequencer stack shared by system tests and developer devnets.

use std::{net::IpAddr, path::PathBuf, time::Duration};

use alloy_genesis::ChainConfig;
use alloy_primitives::B256;
use alloy_rpc_types_engine::JwtSecret;
use base_common_genesis::RollupConfig;
use base_consensus_node::NodeMode;
use base_node_runner::BaseNodeExtension;
use base_upgrade_signal::UpgradeSignalConfig;
use eyre::{Result, WrapErr};
use url::Url;

use super::{
    InProcessBuilder, InProcessBuilderConfig, InProcessConsensus, InProcessConsensusConfig,
};
use crate::SEQUENCER;

/// Configuration for a fresh builder execution node and sequencer consensus node.
#[derive(Debug)]
pub struct SequencerStackConfig {
    /// L2 genesis JSON content.
    pub l2_genesis: Vec<u8>,
    /// Optional persistent builder datadir.
    pub builder_datadir: Option<PathBuf>,
    /// Whether the configured datadir must already contain an initialized database.
    pub require_existing_datadir: bool,
    /// Rollup configuration JSON content.
    pub rollup_config: Vec<u8>,
    /// L1 genesis JSON content.
    pub l1_genesis: Vec<u8>,
    /// JWT secret shared by execution and consensus.
    pub jwt_secret: JwtSecret,
    /// Execution and consensus P2P private key.
    pub p2p_key: B256,
    /// Sequencer block-signing key.
    pub sequencer_key: B256,
    /// L1 execution RPC endpoint.
    pub l1_rpc_url: Url,
    /// L1 beacon API endpoint.
    pub l1_beacon_url: Url,
    /// L1 slot duration override in seconds.
    pub l1_slot_duration: u64,
    /// Address used by execution RPC, metrics, and Flashblocks listeners.
    pub execution_rpc_addr: IpAddr,
    /// Address used by the execution P2P listener and advertised enode.
    pub execution_p2p_addr: IpAddr,
    /// Builder HTTP RPC port.
    pub builder_http_port: Option<u16>,
    /// Builder WebSocket RPC port.
    pub builder_ws_port: Option<u16>,
    /// Builder authenticated Engine API port.
    pub builder_auth_port: Option<u16>,
    /// Builder execution P2P port.
    pub builder_p2p_port: Option<u16>,
    /// Builder Flashblocks WebSocket port.
    pub builder_flashblocks_port: Option<u16>,
    /// Builder Prometheus metrics port.
    pub builder_metrics_port: Option<u16>,
    /// Address used by the consensus RPC listener.
    pub consensus_rpc_addr: IpAddr,
    /// Address used by consensus discovery and gossip listeners.
    pub consensus_p2p_listen_addr: IpAddr,
    /// Address advertised by consensus discovery and gossip.
    pub consensus_p2p_advertise_addr: IpAddr,
    /// Builder consensus RPC port.
    pub consensus_rpc_port: Option<u16>,
    /// Builder consensus P2P TCP port.
    pub consensus_p2p_tcp_port: Option<u16>,
    /// Builder consensus P2P UDP port.
    pub consensus_p2p_udp_port: Option<u16>,
    /// Consensus bootnodes used to join an external devnet network.
    pub consensus_bootnodes: Vec<String>,
    /// Whether the sequencer starts paused for a validator handshake.
    pub sequencer_stopped: bool,
    /// Whether to accept experimental validity transactions.
    pub enable_experimental_validity_transactions: bool,
    /// Whether to cut the builder over at Denim.
    pub payload_builder_cutover: bool,
    /// Optional L1 upgrade signal configuration.
    pub upgrade_signal: Option<UpgradeSignalConfig>,
    /// Additional execution node extensions.
    pub extra_builder_extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Optional pending/basefee/queued transaction count limit.
    pub txpool_max_transactions: Option<usize>,
    /// Optional pending/basefee/queued transaction size limit in megabytes.
    pub txpool_max_size_mb: Option<usize>,
    /// Optional maximum number of transaction slots retained per sender.
    pub txpool_max_account_slots: Option<usize>,
    /// Whether to remove inherited `OpenTelemetry` environment variables before startup.
    pub clear_otel_env: bool,
}

/// A running builder execution node and sequencer consensus node.
#[derive(Debug)]
pub struct SequencerStack {
    builder: InProcessBuilder,
    consensus: InProcessConsensus,
}

impl SequencerStack {
    /// Starts the builder followed by its sequencer consensus node.
    pub async fn start(config: SequencerStackConfig) -> Result<Self> {
        let rollup_config: RollupConfig = serde_json::from_slice(&config.rollup_config)
            .wrap_err("Failed to parse rollup config")?;
        let l1_chain_config: ChainConfig =
            serde_json::from_slice(&config.l1_genesis).wrap_err("Failed to parse L1 genesis")?;
        let chain_spec = InProcessBuilderConfig::chain_spec_from_genesis_json(&config.l2_genesis)
            .wrap_err("Failed to parse builder L2 chain spec")?;

        let builder = InProcessBuilder::start(InProcessBuilderConfig {
            chain_spec,
            datadir: config.builder_datadir,
            require_existing_datadir: config.require_existing_datadir,
            jwt_secret: config.jwt_secret,
            rpc_addr: config.execution_rpc_addr,
            p2p_addr: config.execution_p2p_addr,
            p2p_key: config.p2p_key,
            http_port: config.builder_http_port,
            ws_port: config.builder_ws_port,
            auth_port: config.builder_auth_port,
            p2p_port: config.builder_p2p_port,
            flashblocks_port: config.builder_flashblocks_port,
            metrics_port: config.builder_metrics_port,
            enable_experimental_validity_transactions: config
                .enable_experimental_validity_transactions,
            payload_builder_cutover: config.payload_builder_cutover,
            extra_extensions: config.extra_builder_extensions,
            block_time: Duration::from_secs(rollup_config.block_time),
            persistence_threshold: None,
            txpool_max_transactions: config.txpool_max_transactions,
            txpool_max_size_mb: config.txpool_max_size_mb,
            txpool_max_account_slots: config.txpool_max_account_slots,
            clear_otel_env: config.clear_otel_env,
        })
        .await
        .wrap_err("Failed to start in-process builder")?;

        let consensus = InProcessConsensus::start(InProcessConsensusConfig {
            rollup_config,
            l1_chain_config,
            jwt_secret: config.jwt_secret,
            l1_rpc_url: config.l1_rpc_url,
            l1_beacon_url: config.l1_beacon_url,
            l2_engine_url: builder.engine_url()?,
            mode: NodeMode::Sequencer,
            sequencer_key: Some(config.sequencer_key),
            p2p_key: Some(config.p2p_key),
            rpc_addr: config.consensus_rpc_addr,
            p2p_listen_addr: config.consensus_p2p_listen_addr,
            p2p_advertise_addr: config.consensus_p2p_advertise_addr,
            rpc_port: config.consensus_rpc_port,
            p2p_tcp_port: config.consensus_p2p_tcp_port,
            p2p_udp_port: config.consensus_p2p_udp_port,
            bootnodes: config.consensus_bootnodes,
            unsafe_block_signer: SEQUENCER.address,
            l1_slot_duration_override: Some(config.l1_slot_duration),
            sequencer_stopped: config.sequencer_stopped,
            verifier_l1_confs: 0,
            shadow_blocks_per_cycle: None,
            upgrade_signal: config.upgrade_signal,
        })
        .await
        .wrap_err("Failed to start builder consensus")?;

        Ok(Self { builder, consensus })
    }

    /// Returns the builder execution node.
    pub const fn builder(&self) -> &InProcessBuilder {
        &self.builder
    }

    /// Returns the builder consensus node.
    pub const fn consensus(&self) -> &InProcessConsensus {
        &self.consensus
    }

    /// Stops consensus and gracefully shuts down the builder execution node.
    pub async fn shutdown(self) -> Result<()> {
        drop(self.consensus);
        self.builder.shutdown().await
    }
}
