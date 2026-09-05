//! L2 stack orchestration (Builder + Consensus + Batcher).
//!
//! This module provides [`L2Stack`], which composes a complete L2 network by orchestrating:
//! - Builder execution layer (in-process, produces blocks and sequences transactions)
//! - Consensus layer (in-process, derives L2 blocks from L1 data)
//! - Batcher (in-process, submits L2 transaction batches to L1)
//! - Client execution layer (in-process, follows the L2 and builds pending state using Flashblocks)

use std::{num::NonZeroU64, path::PathBuf, time::Duration};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_genesis::ChainConfig;
use alloy_network::{Ethereum, TransactionBuilder};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::JwtSecret;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionRequest;
use base_consensus_node::NodeMode;
use base_execution_cli::ExecutionUpgradeSignalConfig;
use base_node_runner::BaseNodeExtension;
use base_tx_forwarding::TxForwardingConfig;
use base_upgrade_signal::UpgradeSignalConfig;
use eyre::{Result, WrapErr};
use tokio::time::{sleep, timeout};
use url::Url;

use super::{
    ChainSpecSource, InProcessBatcher, InProcessBatcherConfig, InProcessBuilder,
    InProcessBuilderConfig, InProcessClient, InProcessClientConfig, InProcessConsensus,
    InProcessConsensusConfig, InProcessFollowConsensus, InProcessFollowConsensusConfig,
    L2ContainerConfig, ShadowSequencer, ShadowSequencerConfig,
};
use crate::config::{ANVIL_ACCOUNT_1, BATCHER, SEQUENCER};

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
#[derive(Debug)]
pub struct L2StackConfig {
    /// L2 genesis JSON content.
    pub l2_genesis: Vec<u8>,
    /// Optional caller-owned builder datadir.
    pub builder_datadir: Option<PathBuf>,
    /// Optional caller-owned client datadir.
    pub client_datadir: Option<PathBuf>,
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
    /// L1 slot duration in seconds, used as the consensus derivation poll override.
    pub l1_slot_duration: u64,
    /// Optional container configuration for stable naming and port binding.
    pub container_config: Option<L2ContainerConfig>,
    /// Optional transaction forwarding configuration for the client node.
    /// When set, the client will forward transactions to builder RPC endpoints.
    pub tx_forwarding_config: Option<TxForwardingConfig>,
    /// Whether both L2 nodes enable experimental validity transaction transport,
    /// including `base_sendRawTransactionValidity` on the builder.
    pub enable_experimental_validity_transactions: bool,
    /// Number of L1 blocks to keep distance from the L1 head for the client (validator)
    /// consensus node's derivation pipeline.
    pub verifier_l1_confs: u64,
    /// When set, the in-process batcher posts short-lived calldata channels instead of blobs.
    pub force_batch_submission: bool,
    /// Consensus mode for the L2 client node.
    pub client_consensus_mode: L2ClientConsensusMode,
    /// Optional L1 upgrade signal configuration shared by both consensus nodes.
    pub upgrade_signal: Option<UpgradeSignalConfig>,
    /// Optional L1 upgrade signal configuration for the client execution node.
    pub execution_upgrade_signal: Option<ExecutionUpgradeSignalConfig>,
    /// Shadow sequencer configuration. When [`None`], no shadow sequencers are started.
    pub shadow_sequencers: Option<ShadowSequencersConfig>,
    /// Additional node extensions installed on the builder, after its built-in RPC wiring.
    pub extra_builder_extensions: Vec<Box<dyn BaseNodeExtension>>,
    /// Additional node extensions installed on the client, after its built-in extensions.
    pub extra_client_extensions: Vec<Box<dyn BaseNodeExtension>>,
}

/// Configuration for the shadow sequencers running alongside the active sequencer.
#[derive(Debug, Clone)]
pub struct ShadowSequencersConfig {
    /// Signing keys for shadow sequencers. Each entry spawns one shadow sequencer. Each key must
    /// be distinct from [`L2StackConfig::sequencer_key`] so the shadow's blocks are rejected as
    /// non-canonical by the rest of the network.
    pub keys: Vec<B256>,
    /// Number of private blocks each shadow sequencer builds per reconciliation cycle.
    pub blocks_per_cycle: NonZeroU64,
    /// If set, start the active sequencer first and delay shadows until this L2 height.
    pub start_block: Option<u64>,
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

    /// Returns the follow-mode rollup configuration, when this is a follow-mode consensus node.
    pub fn follow_rollup_config(&self) -> Option<&RollupConfig> {
        match self {
            Self::Validator(_) => None,
            Self::Follow(consensus) => Some(consensus.rollup_config()),
        }
    }

    /// Stops the client consensus task.
    pub async fn shutdown(self) {
        match self {
            Self::Validator(consensus) => drop(consensus),
            Self::Follow(consensus) => consensus.shutdown().await,
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
    shadow_sequencers: Vec<ShadowSequencer>,
}

impl std::fmt::Debug for L2Stack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L2Stack")
            .field("builder", &self.builder)
            .field("builder_consensus", &self.builder_consensus)
            .field("batcher", &self.batcher)
            .field("client", &self.client)
            .field("client_consensus", &self.client_consensus)
            .field("shadow_sequencers", &self.shadow_sequencers)
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

        let mut rollup_config: RollupConfig = serde_json::from_slice(&config.rollup_config)
            .wrap_err("Failed to parse rollup config")?;
        if config.shadow_sequencers.as_ref().is_some_and(|shadow| shadow.start_block.is_some()) {
            rollup_config.block_time = 2;
        }
        let l1_chain_config: ChainConfig = serde_json::from_slice(&config.l1_genesis)
            .wrap_err("Failed to parse L1 chain config")?;
        let builder_chain_spec =
            InProcessBuilderConfig::chain_spec_from_genesis_json(&config.l2_genesis)
                .wrap_err("Failed to parse builder L2 chain spec")?;

        // 1. Start the builder (in-process EL).
        let builder_config = InProcessBuilderConfig {
            chain_spec: builder_chain_spec,
            datadir: config.builder_datadir,
            jwt_secret: config.jwt_secret,
            http_port: container_config.and_then(|c| c.builder_http_port),
            ws_port: container_config.and_then(|c| c.builder_ws_port),
            auth_port: container_config.and_then(|c| c.builder_auth_port),
            p2p_port: container_config.and_then(|c| c.builder_p2p_port),
            flashblocks_port: container_config.and_then(|c| c.builder_flashblocks_port),
            metrics_port: None,
            enable_experimental_validity_transactions: config
                .enable_experimental_validity_transactions,
            extra_extensions: config.extra_builder_extensions,
            block_time: Duration::from_secs(rollup_config.block_time),
            persistence_threshold: None,
            txpool_max_transactions: None,
            txpool_max_size_mb: None,
            txpool_max_account_slots: None,
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
            l1_slot_duration_override: Some(config.l1_slot_duration),
            sequencer_stopped: true,
            verifier_l1_confs: 0,
            shadow_blocks_per_cycle: None,
            upgrade_signal: config.upgrade_signal.clone(),
        };
        let builder_consensus = InProcessConsensus::start(builder_consensus_config)
            .await
            .wrap_err("Failed to start builder consensus")?;

        // 3. Start the normal batcher immediately. Delayed-shadow tests instead start a
        // short-lived, deterministic batcher after producing their historical prefix.
        let delayed_shadow =
            config.shadow_sequencers.as_ref().is_some_and(|c| c.start_block.is_some());
        let mut batcher = if delayed_shadow {
            None
        } else {
            Some(
                InProcessBatcher::start(InProcessBatcherConfig {
                    l1_rpc_url: l1_rpc_url.clone(),
                    l2_rpc_url: builder.rpc_url()?,
                    rollup_rpc_url: builder_consensus.rpc_url(),
                    batcher_key: config.batcher_key,
                    force_batch_submission: config.force_batch_submission,
                })
                .await
                .wrap_err("Failed to start in-process batcher")?,
            )
        };

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
            chain_spec: ChainSpecSource::GenesisJson(config.l2_genesis.clone()),
            datadir: config.client_datadir,
            jwt_secret: config.jwt_secret,
            builder_rpc_url: builder.rpc_url()?.to_string(),
            builder_flashblocks_url: Some(builder.flashblocks_url()),
            builder_p2p_enode: builder.p2p_enode(),
            http_port: container_config.and_then(|c| c.client_http_port),
            ws_port: container_config.and_then(|c| c.client_ws_port),
            auth_port: container_config.and_then(|c| c.client_auth_port),
            p2p_port: container_config.and_then(|c| c.client_p2p_port),
            metrics_port: None,
            persistence_threshold: None,
            tx_forwarding_config,
            enable_experimental_validity_transactions: config
                .enable_experimental_validity_transactions,
            upgrade_signal: config.execution_upgrade_signal.clone(),
            extra_extensions: config.extra_client_extensions,
        };
        let client = InProcessClient::start(client_config)
            .await
            .wrap_err("Failed to start in-process client")?;

        // 5. Start client consensus.
        let client_consensus = match config.client_consensus_mode {
            L2ClientConsensusMode::Validator => {
                let client_consensus_config = InProcessConsensusConfig {
                    rollup_config: rollup_config.clone(),
                    l1_chain_config: l1_chain_config.clone(),
                    jwt_secret: config.jwt_secret,
                    l1_rpc_url: l1_rpc_url.clone(),
                    l1_beacon_url: l1_beacon_url.clone(),
                    l2_engine_url: client.engine_url()?,
                    mode: NodeMode::Validator,
                    sequencer_key: None,
                    p2p_key: None,
                    rpc_port: container_config.and_then(|c| c.client_consensus_rpc_port),
                    p2p_tcp_port: container_config.and_then(|c| c.client_consensus_p2p_tcp_port),
                    p2p_udp_port: container_config.and_then(|c| c.client_consensus_p2p_udp_port),
                    unsafe_block_signer: SEQUENCER.address,
                    l1_slot_duration_override: Some(config.l1_slot_duration),
                    sequencer_stopped: false,
                    verifier_l1_confs: config.verifier_l1_confs,
                    shadow_blocks_per_cycle: None,
                    upgrade_signal: config.upgrade_signal.clone(),
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
                    rollup_config: rollup_config.clone(),
                    jwt_secret: config.jwt_secret,
                    l1_rpc_url: l1_rpc_url.clone(),
                    local_l2_rpc_url: client.rpc_url()?,
                    source_l2_rpc_url: builder.rpc_url()?,
                    l2_engine_url: client.engine_url()?,
                    upgrade_signal: config.upgrade_signal.clone(),
                    rpc_port: container_config.and_then(|c| c.client_consensus_rpc_port),
                    insert_delay: Duration::ZERO,
                };
                let client_consensus = InProcessFollowConsensus::start(client_consensus_config)
                    .await
                    .wrap_err("Failed to start follow client consensus")?;
                L2ClientConsensus::Follow(client_consensus)
            }
        };

        // 6. Unless late startup was requested, shadows join the gossip mesh before active
        // sequencing begins. Preserve that ordering because it is the normal production path.
        if let Some(start_block) = config.shadow_sequencers.as_ref().and_then(|c| c.start_block) {
            let provider = RootProvider::<Base>::new_http(builder.rpc_url()?);
            let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)
                .wrap_err("Failed to parse delayed-shadow batch trigger signer")?;
            let first_nonce = provider.get_transaction_count(signer.address()).await?;
            for offset in 0..start_block {
                let transaction = BaseTransactionRequest::default()
                    .from(signer.address())
                    .to(Address::repeat_byte(0xfe))
                    .value(U256::from(1))
                    .transaction_type(2)
                    .with_gas_limit(21_000)
                    .with_max_fee_per_gas(1_000_000_000)
                    .with_max_priority_fee_per_gas(0)
                    .with_chain_id(rollup_config.l2_chain_id.id())
                    .with_nonce(first_nonce + offset)
                    .build_typed_tx()
                    .map_err(|error| eyre::eyre!("Invalid batch trigger transaction: {error:?}"))?;
                let signature = signer.sign_hash_sync(&transaction.signature_hash())?;
                let raw_transaction: Bytes =
                    transaction.into_signed(signature).encoded_2718().into();
                let pending = provider
                    .send_raw_transaction(&raw_transaction)
                    .await
                    .wrap_err("Failed to submit delayed-shadow batch trigger transaction")?;
                let transaction_hash = *pending.tx_hash();
                drop(pending);
                if offset == 0 {
                    sleep(Duration::from_millis(500)).await;
                    builder_consensus
                        .start_sequencer()
                        .await
                        .wrap_err("Failed to produce delayed-shadow catch-up blocks")?;
                }
                timeout(Duration::from_secs(10), async {
                    while provider.get_transaction_receipt(transaction_hash).await?.is_none() {
                        sleep(Duration::from_millis(10)).await;
                    }
                    Ok::<_, eyre::Error>(())
                })
                .await
                .wrap_err("Timed out waiting for delayed-shadow catch-up receipt")??;
            }
            builder_consensus
                .stop_sequencer()
                .await
                .wrap_err("Failed to pause active sequencer for delayed shadow startup")?;
            batcher = Some(
                InProcessBatcher::start(InProcessBatcherConfig {
                    l1_rpc_url: l1_rpc_url.clone(),
                    l2_rpc_url: builder.rpc_url()?,
                    rollup_rpc_url: builder_consensus.rpc_url(),
                    batcher_key: config.batcher_key,
                    force_batch_submission: true,
                })
                .await
                .wrap_err("Failed to start delayed-shadow batcher")?,
            );
            let safe_target = start_block.saturating_sub(1);
            let safe_wait = timeout(Duration::from_secs(30), async {
                loop {
                    if let Some(error) = batcher.as_ref().and_then(InProcessBatcher::failure) {
                        return Err(eyre::eyre!(
                            "Batcher exited before safe-head catch-up: {error}"
                        ));
                    }
                    let safe_height = provider
                        .get_block_by_number(BlockNumberOrTag::Safe)
                        .await?
                        .map_or(0, |block| block.header.number);
                    if safe_height >= safe_target {
                        return Ok::<_, eyre::Error>(());
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            })
            .await;
            if safe_wait.is_err() {
                let safe_height = provider
                    .get_block_by_number(BlockNumberOrTag::Safe)
                    .await?
                    .map_or(0, |block| block.header.number);
                let l1_provider = RootProvider::<Ethereum>::new_http(l1_rpc_url.clone());
                let batcher_nonce = l1_provider.get_transaction_count(BATCHER.address).await?;
                eyre::bail!(
                    "Timed out waiting for active sequencer safe head before delayed shadows: \
                     safe={safe_height}, target={safe_target}, batcher_nonce={batcher_nonce}"
                );
            }
            safe_wait.expect("timeout handled")?;
        }

        let active_consensus_p2p_addr = builder_consensus.p2p_addr();
        let mut shadow_sequencers = Vec::new();
        if let Some(shadow_config) = &config.shadow_sequencers {
            shadow_sequencers.reserve(shadow_config.keys.len());
            for (index, shadow_key) in shadow_config.keys.iter().enumerate() {
                let shadow = ShadowSequencer::start(ShadowSequencerConfig {
                    sequencer_key: *shadow_key,
                    l2_genesis: config.l2_genesis.clone(),
                    rollup_config: rollup_config.clone(),
                    l1_chain_config: l1_chain_config.clone(),
                    jwt_secret: config.jwt_secret,
                    l1_rpc_url: l1_rpc_url.clone(),
                    l1_beacon_url: l1_beacon_url.clone(),
                    active_consensus_p2p_addr: active_consensus_p2p_addr.clone(),
                    active_sequencer_address: SEQUENCER.address,
                    shadow_blocks_per_cycle: shadow_config.blocks_per_cycle,
                    l1_slot_duration: config.l1_slot_duration,
                })
                .await
                .wrap_err_with(|| format!("Failed to start shadow sequencer {index}"))?;
                let shadow_start_safe_height = if shadow_config.start_block.is_some() {
                    Some(
                        RootProvider::<Base>::new_http(builder.rpc_url()?)
                            .get_block_by_number(BlockNumberOrTag::Safe)
                            .await?
                            .map_or(0, |block| block.header.number),
                    )
                } else {
                    None
                };
                if let Some(shadow_start_safe_height) = shadow_start_safe_height {
                    let shadow_provider = RootProvider::<Base>::new_http(shadow.rpc_url()?);
                    timeout(Duration::from_secs(30), async {
                        while shadow_provider
                            .get_block_by_number(BlockNumberOrTag::Safe)
                            .await?
                            .map_or(0, |block| block.header.number)
                            < shadow_start_safe_height
                        {
                            sleep(Duration::from_millis(100)).await;
                        }
                        Ok::<_, eyre::Error>(())
                    })
                    .await
                    .wrap_err("Timed out waiting for delayed shadow safe catch-up")??;
                    batcher.as_ref().expect("delayed batcher started").stop();
                    builder_consensus.start_sequencer().await.wrap_err(
                        "Failed to resume active sequencer after delayed shadow startup",
                    )?;
                }
                shadow_sequencers.push(shadow);
            }
        }

        // 7. In the default path, start active sequencing only after every shadow connected.
        if !delayed_shadow {
            builder_consensus
                .start_sequencer()
                .await
                .wrap_err("Failed to start sequencer after peer connection")?;
        }

        Ok(Self {
            builder,
            builder_consensus,
            batcher: batcher.expect("batcher starts in both shadow startup paths"),
            client,
            client_consensus,
            shadow_sequencers,
        })
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

    /// Returns the shadow sequencers running alongside the active sequencer.
    pub fn shadow_sequencers(&self) -> &[ShadowSequencer] {
        &self.shadow_sequencers
    }

    /// Returns the shadow sequencer at `index`, if present.
    pub fn shadow_sequencer(&self, index: usize) -> Option<&ShadowSequencer> {
        self.shadow_sequencers.get(index)
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

    /// Returns the follow-mode client consensus rollup configuration, when enabled.
    pub fn client_follow_rollup_config(&self) -> Option<&RollupConfig> {
        self.client_consensus.follow_rollup_config()
    }

    /// Stops the in-process L2 services and gracefully shuts down execution runtimes.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            builder,
            builder_consensus,
            batcher,
            client,
            client_consensus,
            shadow_sequencers,
        } = self;

        for shadow in shadow_sequencers {
            shadow.shutdown().await.wrap_err("Failed to shut down shadow sequencer")?;
        }

        client_consensus.shutdown().await;
        drop(batcher);
        drop(builder_consensus);

        client.shutdown().await.wrap_err("Failed to shut down L2 client")?;
        builder.shutdown().await.wrap_err("Failed to shut down L2 builder")?;

        Ok(())
    }
}
