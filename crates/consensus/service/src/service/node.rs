//! Contains the [`RollupNode`] implementation.
use std::{
    ops::Not as _,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use alloy_eips::BlockNumberOrTag;
use alloy_genesis::ChainConfig as GenesisChainConfig;
use alloy_primitives::Address;
use alloy_provider::RootProvider;
use base_common_chains::ChainConfig;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types_engine::PayloadReady;
use base_consensus_derive::{Pipeline, SignalReceiver, StatefulAttributesBuilder};
use base_consensus_engine::{Engine, EngineClient, EngineState, ForkchoiceCheckpointReader};
use base_consensus_providers::{
    AlloyChainProvider, AlloyL2ChainProvider, OnlineBeaconClient, OnlineBlobProvider,
    OnlinePipeline,
};
use base_consensus_rpc::{AdminNetworkAccess, BaseRpc, RpcBuilder};
use base_consensus_safedb::{DisabledSafeDB, SafeDB, SafeDBReader, SafeHeadListener};
use base_protocol::L2BlockInfo;
use base_upgrade_signal::{UpgradeSignalMetricLayer, UpgradeSignalMetrics};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    AlloyL1BlockFetcher, CheckpointActor, CheckpointClient, CheckpointDB, CheckpointWriter,
    Conductor, ConductorClient, DelayedL1OriginSelectorProvider, DerivationActor, EngineActor,
    EngineActorRequest, EngineConfig, EngineProcessor, EngineRequestReceiver, EngineRpcProcessor,
    L1OriginSelector, L1WatcherActor, L1WatcherQueryExecutor, L1WatcherQueryProcessor,
    NetworkActor, NetworkBuilder, NetworkConfig, NodeActor, NodeOperatingMode, PayloadBuilder,
    PrefetchedChainProvider, PreparedL1Origin, QueuedDerivationEngineClient,
    QueuedEngineDerivationClient, QueuedEngineRpcClient, QueuedL1WatcherDerivationClient,
    QueuedNetworkEngineClient, QueuedSequencerAdminAPIClient, QueuedSequencerEngineClient,
    RecoveryModeGuard, RpcActor, RpcContext, SequencerActor, SequencerConfig,
    SequencerEngineRequestCoordinator, UpgradeSignalNodeConfig, ValidatorEngineRequestHandler,
    actors::{BlockStream, NetworkInboundData, QueuedUnsafePayloadGossipClient},
};

const DERIVATION_PROVIDER_CACHE_SIZE: usize = 1024;

#[derive(Debug)]
enum ConfiguredEngineReceiver<E: EngineClient + 'static> {
    Validator(ValidatorEngineRequestHandler<E, QueuedEngineDerivationClient>),
    Sequencer(SequencerEngineRequestCoordinator<E, QueuedEngineDerivationClient>),
}

impl<E: EngineClient + 'static> EngineRequestReceiver for ConfiguredEngineReceiver<E> {
    fn start(
        self,
        request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> tokio::task::JoinHandle<Result<(), crate::EngineError>> {
        match self {
            Self::Validator(receiver) => receiver.start(request_channel),
            Self::Sequencer(receiver) => receiver.start(request_channel),
        }
    }
}
/// Poll interval in seconds for the head block stream.
pub const HEAD_STREAM_POLL_INTERVAL: u64 = 4;

/// The configuration for the L1 chain.
#[derive(Debug, Clone)]
pub struct L1Config {
    /// The L1 chain configuration.
    pub chain_config: Arc<GenesisChainConfig>,
    /// Whether to trust the L1 RPC.
    pub trust_rpc: bool,
    /// The L1 beacon API client.
    pub beacon_client: OnlineBeaconClient,
    /// The general L1 execution JSON-RPC provider.
    pub engine_provider: RootProvider,
    /// How frequently to poll L1 for new finalized and safe blocks, both of which advance once
    /// per L1 epoch.
    ///
    /// The right value depends on the L1 finality cadence:
    /// - Ethereum mainnet/Sepolia: one epoch (~384 s = 32 slots × 12 s)
    /// - Devnet local L1: near-instant finality, poll aggressively (~2 s)
    pub finalized_poll_interval: Duration,
    /// Number of L1 blocks to keep distance from the L1 head for the verifier (derivation
    /// pipeline). When non-zero, the L1 watcher delays derivation heads by this many blocks,
    /// providing reorg protection. Controlled via `BASE_NODE_VERIFIER_L1_CONFS`.
    pub verifier_l1_confs: u64,
    /// Optional sender used only to filter L1 data-availability transactions.
    pub da_batcher_sender_override: Option<Address>,
}

impl L1Config {
    /// Returns the recommended finalized-block poll interval for the given L1 chain.
    pub const fn default_finalized_poll_interval(l1_chain_id: u64) -> Duration {
        const ETH_MAINNET_L1: u64 = ChainConfig::mainnet().l1_chain_id;
        const ETH_SEPOLIA_L1: u64 = ChainConfig::sepolia().l1_chain_id;
        const DEVNET_L1: u64 = ChainConfig::devnet().l1_chain_id;

        match l1_chain_id {
            // Ethereum mainnet and Sepolia: poll once per L1 epoch (32 slots × 12 s).
            ETH_MAINNET_L1 | ETH_SEPOLIA_L1 => Duration::from_secs(384),
            // Devnet local L1: near-instant finality, poll aggressively.
            DEVNET_L1 => Duration::from_secs(2),
            // Unknown chains: fall back to a conservative default.
            _ => Duration::from_secs(60),
        }
    }
}

/// The standard implementation of the [`RollupNode`] service, using the governance approved Base
/// configuration of components.
#[derive(Debug)]
pub struct RollupNode {
    /// The rollup configuration.
    pub config: Arc<RollupConfig>,
    /// The L1 configuration.
    pub l1_config: L1Config,
    /// L1 execution JSON-RPC provider dedicated to the sequencer block-production hot path.
    pub sequencer_l1_provider: RootProvider,
    /// The L2 EL provider.
    pub l2_provider: RootProvider<Base>,
    /// Whether to trust the L2 RPC.
    pub l2_trust_rpc: bool,
    /// The [`EngineConfig`] for the node.
    pub engine_config: EngineConfig,
    /// The [`RpcBuilder`] for the node.
    pub rpc_builder: Option<RpcBuilder>,
    /// The P2P [`NetworkConfig`] for the node.
    pub p2p_config: NetworkConfig,
    /// The [`SequencerConfig`] for the node.
    pub sequencer_config: SequencerConfig,
    /// Path to the mandatory checkpoint database.
    ///
    /// The node records safe/finalized forkchoice checkpoints here so restart can recover
    /// `L2BlockInfo` when reth has pruned old block bodies.
    pub checkpoint_path: PathBuf,
    /// Optional path to the safe head database.
    ///
    /// When set, the node records L1→L2 safe head mappings to a persistent redb database and
    /// serves them via the `optimism_safeHeadAtL1Block` RPC endpoint. When `None`, safe head
    /// tracking is disabled and that RPC method returns an error.
    ///
    /// If the path is set but the database cannot be opened (e.g., bad permissions, disk
    /// error, or corrupted file), the node **fails to start** with an error.
    pub safedb_path: Option<PathBuf>,
    /// Optional upgrade signal configuration for the consensus node.
    pub upgrade_signal_config: Option<UpgradeSignalNodeConfig>,
    /// Optional frozen-payload hints from an embedded execution node.
    pub payload_ready: Option<broadcast::Receiver<PayloadReady>>,
}

impl RollupNode {
    const fn mode(&self) -> NodeOperatingMode {
        self.engine_config.mode
    }

    /// Creates a network builder for the node.
    fn network_builder(&self) -> NetworkBuilder {
        NetworkBuilder::from(self.p2p_config.clone())
    }

    /// Returns an engine builder for the node.
    fn engine_config(&self) -> EngineConfig {
        self.engine_config.clone()
    }

    /// Returns an rpc builder for the node.
    fn rpc_builder(&self) -> Option<RpcBuilder> {
        self.rpc_builder.clone()
    }

    /// Returns the sequencer attributes builder for the node.
    fn create_attributes_builder(
        &self,
        origin_rx: watch::Receiver<Option<PreparedL1Origin>>,
    ) -> StatefulAttributesBuilder<PrefetchedChainProvider, AlloyL2ChainProvider> {
        let l1_fallback_provider = AlloyChainProvider::new_with_trust(
            self.sequencer_l1_provider.clone(),
            DERIVATION_PROVIDER_CACHE_SIZE,
            self.l1_config.trust_rpc,
        );
        let l1_derivation_provider = PrefetchedChainProvider::new(origin_rx, l1_fallback_provider);
        let l2_derivation_provider = AlloyL2ChainProvider::new_with_trust(
            self.l2_provider.clone(),
            Arc::clone(&self.config),
            DERIVATION_PROVIDER_CACHE_SIZE,
            self.l2_trust_rpc,
        );

        StatefulAttributesBuilder::new(
            Arc::clone(&self.config),
            Arc::clone(&self.l1_config.chain_config),
            l2_derivation_provider,
            l1_derivation_provider,
        )
    }

    async fn create_pipeline(
        &self,
        l1_head_number: base_consensus_providers::L1HeadNumber,
    ) -> OnlinePipeline {
        // Create the caching L1/L2 EL providers for derivation.
        let l1_derivation_provider = AlloyChainProvider::new_with_trust(
            self.l1_config.engine_provider.clone(),
            DERIVATION_PROVIDER_CACHE_SIZE,
            self.l1_config.trust_rpc,
        );
        let l2_derivation_provider = AlloyL2ChainProvider::new_with_trust(
            self.l2_provider.clone(),
            Arc::clone(&self.config),
            DERIVATION_PROVIDER_CACHE_SIZE,
            self.l2_trust_rpc,
        );

        OnlinePipeline::new_polled_with_da_batcher_sender_override(
            Arc::clone(&self.config),
            Arc::clone(&self.l1_config.chain_config),
            OnlineBlobProvider::init(self.l1_config.beacon_client.clone()).await,
            l1_derivation_provider,
            l2_derivation_provider,
            l1_head_number,
            self.l1_config.verifier_l1_confs,
            self.l1_config.da_batcher_sender_override,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_engine_actor<E: EngineClient + 'static>(
        &self,
        engine_client: Arc<E>,
        cancellation_token: CancellationToken,
        engine_request_rx: mpsc::Receiver<EngineActorRequest>,
        derivation_client: QueuedEngineDerivationClient,
        unsafe_head_tx: watch::Sender<L2BlockInfo>,
        conductor: Option<Arc<dyn Conductor>>,
        checkpoint_client: CheckpointClient,
    ) -> Result<(
        EngineActor<impl EngineRequestReceiver + use<E>>,
        EngineRpcProcessor<E>,
        watch::Receiver<EngineState>,
    ), crate::EngineClientError> {
        let engine_state = EngineState::default();
        let (engine_state_tx, engine_state_rx) = watch::channel(engine_state);
        let sequencer_engine_state_rx = engine_state_rx.clone();
        let (engine_queue_length_tx, engine_queue_length_rx) = watch::channel(0);
        let engine = Engine::new(engine_state, engine_state_tx, engine_queue_length_tx);

        let node_mode = self.mode();
        if self.sequencer_config.historical_el_sync_target.is_some() && node_mode.is_validator() {
            return Err(crate::EngineClientError::HistoricalStartupRefused(
                "historical execution startup requires regular sequencer mode",
            ));
        }
        let checkpoint_reader: Arc<dyn ForkchoiceCheckpointReader> =
            Arc::new(checkpoint_client.clone());
        let checkpoint_writer: Arc<dyn CheckpointWriter> = Arc::new(checkpoint_client);
        let engine_processor = EngineProcessor::new_with_checkpoint(
            Arc::clone(&engine_client),
            Arc::clone(&self.config),
            derivation_client,
            engine,
            checkpoint_reader,
            checkpoint_writer,
        );

        let engine_rpc_processor = EngineRpcProcessor::new(
            Arc::clone(&engine_client),
            Arc::clone(&self.config),
            engine_state_rx,
            engine_queue_length_rx,
        );

        let engine_handler = if node_mode.is_validator() {
            ConfiguredEngineReceiver::Validator(ValidatorEngineRequestHandler::new(
                engine_processor,
            ))
        } else {
            let mut coordinator = SequencerEngineRequestCoordinator::new(
                engine_processor,
                node_mode,
                conductor,
                self.sequencer_config.sequencer_stopped,
                unsafe_head_tx,
            );
            if let Some(target) = self.sequencer_config.historical_el_sync_target {
                coordinator.begin_historical_startup(target)?;
            }
            ConfiguredEngineReceiver::Sequencer(coordinator)
        };
        let engine_actor = EngineActor::new(cancellation_token, engine_request_rx, engine_handler);

        Ok((engine_actor, engine_rpc_processor, sequencer_engine_state_rx))
    }

    /// Starts the rollup node service.
    ///
    /// The rollup node, in validator mode, listens to two sources of information to sync the L2
    /// chain:
    ///
    /// 1. The data availability layer, with a watcher that listens for new updates. L2 inputs (L2
    ///    transaction batches + deposits) are then derived from the DA layer.
    /// 2. The L2 sequencer, which produces unsafe L2 blocks and sends them to the network over p2p
    ///    gossip.
    ///
    /// From these two sources, the node imports `unsafe` blocks from the L2 sequencer, `safe`
    /// blocks from the L2 derivation pipeline into the L2 execution layer via the Engine API,
    /// and finalizes `safe` blocks that it has derived when L1 finalized block updates are
    /// received.
    ///
    /// In sequencer mode, the node is responsible for producing unsafe L2 blocks and sending them
    /// to the network over p2p gossip. The node also listens for L1 finalized block updates and
    /// finalizes `safe` blocks that it has derived when L1 finalized block updates are
    /// received.
    pub async fn start(&self) -> Result<(), String> {
        self.start_with_cancellation(CancellationToken::new()).await
    }

    /// Starts the rollup node service with a caller-provided cancellation token.
    pub async fn start_with_cancellation(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), String> {
        let l1_head_number: base_consensus_providers::L1HeadNumber = Arc::new(AtomicU64::new(0));
        let pipeline = self.create_pipeline(Arc::clone(&l1_head_number)).await;
        let engine_client =
            Arc::new(self.engine_config().build_engine_client().await.map_err(|e| e.to_string())?);
        self.start_inner(engine_client, pipeline, l1_head_number, cancellation).await
    }

    async fn start_inner<E, P>(
        &self,
        engine_client: Arc<E>,
        pipeline: P,
        l1_head_number: base_consensus_providers::L1HeadNumber,
        cancellation: CancellationToken,
    ) -> Result<(), String>
    where
        E: EngineClient + 'static,
        P: Pipeline + SignalReceiver + Send + Sync + 'static,
    {
        UpgradeSignalMetrics::record_mode(
            UpgradeSignalMetricLayer::Consensus,
            self.upgrade_signal_config.as_ref().map(|config| config.config.mode),
        );

        // Build the safe head DB pair. Both actors share the same underlying DB via Arc.
        let (safe_head_listener, safe_db_reader): (
            Arc<dyn SafeHeadListener>,
            Arc<dyn SafeDBReader>,
        ) = if let Some(path) = &self.safedb_path {
            let db = Arc::new(
                SafeDB::open(path)
                    .map_err(|e| format!("failed to open safe head database: {e}"))?,
            );
            (Arc::clone(&db) as Arc<dyn SafeHeadListener>, db as Arc<dyn SafeDBReader>)
        } else {
            let db = Arc::new(DisabledSafeDB);
            (Arc::clone(&db) as Arc<dyn SafeHeadListener>, db as Arc<dyn SafeDBReader>)
        };

        let (derivation_actor_request_tx, derivation_actor_request_rx) = mpsc::channel(1024);

        let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(1024);
        let (engine_rpc_request_tx, engine_rpc_request_rx) = mpsc::channel(1024);
        let (unsafe_head_tx, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
        let (derivation_origin_tx, derivation_origin_rx) = watch::channel(None);
        let (checkpoint_request_tx, checkpoint_request_rx) = mpsc::channel(1024);
        let checkpoint_db = CheckpointDB::open(&self.checkpoint_path)
            .map_err(|e| format!("failed to open checkpoint database: {e}"))?;
        let checkpoint_actor = CheckpointActor::new(checkpoint_db, checkpoint_request_rx);
        let checkpoint_client = CheckpointClient::new(checkpoint_request_tx);

        // Create the conductor client early — the engine processor needs it for the
        // bootstrap leadership check and the sequencer actor needs it for block building.
        // When `conductor_binary_commit` is set, commit_unsafe_payload uses the
        // SSZ-binary endpoint; the other RPCs (leader, active, override_leader)
        // continue to use JSON-RPC.
        let binary_commit = self.sequencer_config.conductor_binary_commit;
        let conductor_timeout = self.sequencer_config.conductor_rpc_timeout;
        let conductor: Option<ConductorClient> = self
            .sequencer_config
            .conductor_rpc_url
            .clone()
            .map(|url| {
                if binary_commit {
                    ConductorClient::new_http_with_binary_commit(url, conductor_timeout)
                } else {
                    ConductorClient::new_http(url, conductor_timeout)
                }
            })
            .transpose()
            .map_err(|e| format!("Failed to create conductor client: {e}"))?;

        let engine_conductor: Option<Arc<dyn Conductor>> =
            conductor.clone().map(|c| Arc::new(c) as Arc<dyn Conductor>);

        let engine_derivation_client =
            QueuedEngineDerivationClient::new(derivation_actor_request_tx.clone());
        let (engine_actor, engine_rpc_processor, sequencer_engine_state_rx) = self
            .create_engine_actor(
                engine_client,
                cancellation.clone(),
                engine_actor_request_rx,
                engine_derivation_client,
                unsafe_head_tx,
                engine_conductor,
                checkpoint_client,
            ).map_err(|error| error.to_string())?;

        let derivation = DerivationActor::new(
            QueuedDerivationEngineClient {
                engine_actor_request_tx: engine_actor_request_tx.clone(),
            },
            cancellation.clone(),
            derivation_actor_request_rx,
            pipeline,
            safe_head_listener,
            derivation_origin_tx,
        );

        // Create the p2p actor.
        let (
            NetworkInboundData {
                signer,
                p2p_rpc: network_rpc,
                gossip_payload_tx,
                admin_rpc: net_admin_rpc,
            },
            network,
        ) = NetworkActor::new(
            QueuedNetworkEngineClient { engine_actor_request_tx: engine_actor_request_tx.clone() },
            cancellation.clone(),
            self.network_builder(),
        )
        .await
        .map_err(|e| format!("Failed to start network actor: {e}"))?;

        let (l1_head_updates_tx, l1_head_updates_rx) = watch::channel(None);

        // Create the L1 Watcher actor

        // A channel to send queries about the state of L1.
        let (l1_query_tx, l1_query_rx) = mpsc::channel(1024);

        let head_stream = BlockStream::new_as_stream(
            self.l1_config.engine_provider.clone(),
            BlockNumberOrTag::Latest,
            Duration::from_secs(HEAD_STREAM_POLL_INTERVAL),
        )?;
        let safe_stream = BlockStream::new_as_stream(
            self.l1_config.engine_provider.clone(),
            BlockNumberOrTag::Safe,
            self.l1_config.finalized_poll_interval,
        )?;
        let finalized_stream = BlockStream::new_as_stream(
            self.l1_config.engine_provider.clone(),
            BlockNumberOrTag::Finalized,
            self.l1_config.finalized_poll_interval,
        )?;
        let (l1_safe_tx, l1_safe_rx) = watch::channel(None);
        let (l1_finalized_tx, l1_finalized_rx) = watch::channel(None);

        // Create the [`L1WatcherActor`]. Previously known as the DA watcher actor.
        let l1_watcher = L1WatcherActor::new(
            Arc::clone(&self.config),
            AlloyL1BlockFetcher(self.l1_config.engine_provider.clone()),
            l1_head_updates_tx.clone(),
            l1_safe_tx,
            l1_finalized_tx,
            QueuedL1WatcherDerivationClient::new(derivation_actor_request_tx),
            Some(signer),
            cancellation.clone(),
            head_stream,
            safe_stream,
            finalized_stream,
            self.l1_config.verifier_l1_confs,
            l1_head_number,
        );
        let l1_query_processor = L1WatcherQueryProcessor::new(
            L1WatcherQueryExecutor::new(
                Arc::clone(&self.config),
                l1_head_updates_tx.subscribe(),
                l1_safe_rx,
                l1_finalized_rx,
                derivation_origin_rx,
            ),
            l1_query_rx,
            cancellation.clone(),
        );
        // One refresher instance is shared between the metrics actor (live auto-apply) and the
        // sequencer admin RPC.
        let upgrade_signal_refresher =
            self.upgrade_signal_config.as_ref().and_then(|c| c.refresher());
        let upgrade_signal_metrics_actor = self
            .upgrade_signal_config
            .as_ref()
            .map(|c| c.metrics_actor(upgrade_signal_refresher.clone(), cancellation.clone()));
        let node_mode = self.mode();
        // Create the sequencer if needed
        let (sequencer_actor, sequencer_admin_client) = if node_mode.is_sequencer() {
            let delayed_l1_provider = DelayedL1OriginSelectorProvider::new(
                self.sequencer_l1_provider.clone(),
                l1_head_updates_rx,
                self.sequencer_config.l1_conf_delay,
            );
            let delayed_origin_selector =
                L1OriginSelector::new(Arc::clone(&self.config), delayed_l1_provider);
            let attributes_builder =
                self.create_attributes_builder(delayed_origin_selector.subscribe());
            let sequencer_engine_client = QueuedSequencerEngineClient {
                engine_actor_request_tx: engine_actor_request_tx.clone(),
                unsafe_head_rx,
                engine_state_rx: sequencer_engine_state_rx,
            };

            // Create the admin API channel
            let (sequencer_admin_api_tx, sequencer_admin_api_rx) = mpsc::channel(1024);
            let queued_gossip_client = if node_mode.is_isolated() {
                QueuedUnsafePayloadGossipClient::private()
            } else {
                QueuedUnsafePayloadGossipClient::new(gossip_payload_tx)
            };

            let recovery_mode =
                RecoveryModeGuard::new(self.sequencer_config.sequencer_recovery_mode);
            let engine_client = Arc::new(sequencer_engine_client);
            (
                Some(SequencerActor {
                    admin_api_rx: sequencer_admin_api_rx,
                    builder: PayloadBuilder {
                        attributes_builder,
                        engine_client: Arc::clone(&engine_client),
                        origin_selector: delayed_origin_selector,
                        recovery_mode: recovery_mode.clone(),
                        rollup_config: Arc::clone(&self.config),
                    },
                    cancellation_token: cancellation.clone(),
                    conductor,
                    engine_client,
                    is_active: self.sequencer_config.sequencer_stopped.not(),
                    mode: node_mode,
                    shadow_funding: self.sequencer_config.shadow_funding,
                    recovery_mode,
                    rollup_config: Arc::clone(&self.config),
                    seal_offset: self.sequencer_config.seal_offset,
                    schedule_delay: Duration::ZERO,
                    unsafe_payload_gossip_client: queued_gossip_client,
                    sealer: None,
                    pending_stop: None,
                    payload_ready: self
                        .payload_ready
                        .as_ref()
                        .map(broadcast::Receiver::resubscribe),
                }),
                Some(QueuedSequencerAdminAPIClient::new(sequencer_admin_api_tx)),
            )
        } else {
            (None, None)
        };

        // Create the RPC server actor.
        let rpc_builder = self.rpc_builder();
        let engine_rpc_actor = rpc_builder
            .as_ref()
            .map(|_| (engine_rpc_processor, (cancellation.clone(), engine_rpc_request_rx)));
        // Public `base` namespace, available whenever the upgrade signal is configured (any mode),
        // built from the same config and reader the node uses. The readiness report accounts for the
        // configured mode, so a node whose mode does not apply the live schedule is not reported
        // ready for it.
        let base_rpc = self
            .upgrade_signal_config
            .as_ref()
            .map(|c| BaseRpc::new(c.config.clone(), c.reader.clone()));
        let rpc = rpc_builder.map(|b| {
            RpcActor::new(
                b,
                QueuedEngineRpcClient::new(engine_rpc_request_tx),
                sequencer_admin_client,
                safe_db_reader,
                upgrade_signal_refresher,
                base_rpc,
            )
        });

        crate::service::spawn_and_wait!(
            cancellation,
            actors = [
                rpc.map(|r| (
                    r,
                    RpcContext {
                        cancellation: cancellation.clone(),
                        p2p_network: Some(network_rpc),
                        admin_network_access: if node_mode.is_isolated() {
                            AdminNetworkAccess::Disabled
                        } else {
                            AdminNetworkAccess::Enabled(net_admin_rpc)
                        },
                        l1_watcher_queries: l1_query_tx,
                    }
                )),
                sequencer_actor.map(|s| (s, ())),
                Some((network, ())),
                Some((l1_watcher, ())),
                Some((l1_query_processor, ())),
                upgrade_signal_metrics_actor.map(|actor| (actor, ())),
                Some((derivation, ())),
                Some((checkpoint_actor, cancellation.clone())),
                Some((engine_actor, ())),
                engine_rpc_actor,
            ]
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::{IpAddr, Ipv4Addr}, sync::Arc, time::Duration};

    use alloy_eips::BlockNumberOrTag;
    use alloy_genesis::ChainConfig;
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, JwtSecret, PayloadStatus, PayloadStatusEnum};
    use base_common_genesis::RollupConfig;
    use base_consensus_disc::LocalNode;
    use base_consensus_engine::{EngineEndpoint, test_utils::{test_block_info, test_engine_client_builder}};
    use discv5::enr::k256::ecdsa::SigningKey;
    use rstest::rstest;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::RollupNode;
    use crate::{CheckpointClient, EngineActorRequest, EngineClientError, EngineConfig, L1ConfigBuilder, NetworkConfig, NodeActor, NodeOperatingMode, QueuedEngineDerivationClient, RollupNodeBuilder, SequencerConfig};

    /// Builds lazy node configuration without opening sockets or starting unrelated actors.
    async fn historical_startup_node(mode: NodeOperatingMode, target: Option<base_protocol::L2BlockInfo>) -> RollupNode {
        const TRUST_RPC: bool = true;
        let config = RollupConfig::default();
        let endpoint = Url::parse("http://127.0.0.1:1").expect("lazy test URL must parse");
        let discovery = LocalNode::new(
            SigningKey::from_bytes((&[7_u8; 32]).into()).expect("test discovery key must decode"),
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 0,
        );
        let network = NetworkConfig::new(config.clone(), discovery, "/ip4/127.0.0.1/tcp/0".parse().expect("test gossip address must parse"), Address::ZERO);
        RollupNodeBuilder::new(
            config.clone(),
            L1ConfigBuilder {
                chain_config: ChainConfig::default(), trust_rpc: TRUST_RPC,
                beacon: endpoint.clone(), rpc_url: endpoint.clone(),
                rpc_timeout: Duration::from_secs(1), slot_duration_override: None,
                verifier_l1_confs: 0, da_batcher_sender_override: None,
            },
            TRUST_RPC,
            EngineConfig {
                config: Arc::new(config), l2_endpoint: EngineEndpoint::Url(endpoint.clone()),
                l2_jwt_secret: JwtSecret::random(), l1_url: endpoint,
                l1_rpc_timeout: Duration::from_secs(1), mode,
            },
            network, None,
        ).with_sequencer_config(SequencerConfig {
            sequencer_stopped: true, historical_el_sync_target: target, ..Default::default()
        }).build().await.expect("lazy node construction must not require a running RPC server")
    }

    /// The production constructor must select historical routing only for a configured sequencer.
    #[rstest]
    #[case::default_off(NodeOperatingMode::Sequencer, None)]
    #[case::selected(NodeOperatingMode::Sequencer, Some(test_block_info(100)))]
    #[case::validator_refused(NodeOperatingMode::Validator, Some(test_block_info(100)))]
    #[tokio::test]
    async fn create_engine_actor_historical_startup_wiring(
        #[case] mode: NodeOperatingMode,
        #[case] target: Option<base_protocol::L2BlockInfo>,
    ) {
        let node = historical_startup_node(mode, target).await;
        let local_head = test_block_info(1);
        let client = Arc::new(test_engine_client_builder().with_block_info_by_tag(BlockNumberOrTag::Latest, local_head).build());
        client.set_fork_choice_updated_v3_response(ForkchoiceUpdated { payload_status: PayloadStatus { status: PayloadStatusEnum::Syncing, latest_valid_hash: None }, payload_id: None }).await;
        let storage = client.storage();
        let cancellation = CancellationToken::new();
        let (request_tx, request_rx) = mpsc::channel(8);
        let (derivation_tx, _derivation_rx) = mpsc::channel(8);
        let (unsafe_head_tx, _unsafe_head_rx) = watch::channel(local_head);
        let (checkpoint_tx, _checkpoint_rx) = mpsc::channel(8);
        let result = node.create_engine_actor(
            Arc::clone(&client), cancellation.clone(), request_rx,
            QueuedEngineDerivationClient::new(derivation_tx), unsafe_head_tx, None,
            CheckpointClient::new(checkpoint_tx),
        );
        if mode.is_validator() {
            assert!(matches!(result, Err(EngineClientError::HistoricalStartupRefused("historical execution startup requires regular sequencer mode"))), "the actual constructor must reject a validator with a historical target");
            assert_eq!(client.storage().read().await.fork_choice_updated_v3_requests.len(), 0, "validator refusal must precede every engine FCU");
            return;
        }
        let (actor, _rpc, _state) = result.expect("stopped regular sequencer must construct successfully");
        let (result_tx, mut result_rx) = mpsc::channel(1);
        request_tx.send(EngineActorRequest::PrepareSequencerStart { expected_hash: local_head.block_info.hash, result_tx }).await.expect("activation query must enqueue");
        let handle = tokio::spawn(actor.start(()));
        let response = tokio::time::timeout(Duration::from_secs(1), result_rx.recv()).await.expect("constructed actor must answer without waiting for the startup deadline").expect("activation response channel must remain open");
        if let Some(target) = target {
            assert!(matches!(response, Err(EngineClientError::HistoricalStartupPending)), "a configured target must select the historical phase at the production wiring point");
            tokio::time::timeout(Duration::from_secs(1), async {
                while client.storage().read().await.fork_choice_updated_v3_requests.is_empty() {
                    tokio::task::yield_now().await;
                }
            }).await.expect("the selected phase must issue its immediate target probe");
            let storage = storage.read().await;
            assert_eq!(storage.fork_choice_updated_v3_requests.len(), 1, "historical startup must issue exactly one initial probe before the next cadence tick");
            let (forkchoice, has_attributes) = &storage.fork_choice_updated_v3_requests[0];
            assert_eq!(*forkchoice, ForkchoiceState { head_block_hash: target.block_info.hash, safe_block_hash: B256::ZERO, finalized_block_hash: B256::ZERO }, "the constructor must preserve the exact configured target FCU with zero safe/finalized hashes");
            assert!(!has_attributes, "historical startup must not submit payload attributes");
        } else {
            assert!(matches!(response, Err(EngineClientError::ELSyncing)), "without a target the stopped follower must retain ordinary bootstrap routing, not historical admission");
            let storage = storage.read().await;
            assert_eq!(storage.fork_choice_updated_v3_requests.len(), 1, "default-off construction must retain exactly the ordinary local-head bootstrap FCU");
            assert_eq!(storage.fork_choice_updated_v3_requests[0].0.head_block_hash, local_head.block_info.hash, "without a target the bootstrap must probe the local head rather than a historical target");
        }
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle).await.expect("constructed actor must stop promptly").expect("constructed actor must not panic").expect("cancellation must shut the actor down cleanly");
    }
}
