use std::{sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::RootProvider;
use base_alloy_network::Base;
use base_consensus_engine::{Engine, EngineClient, EngineState};
use base_consensus_genesis::RollupConfig;
use base_consensus_rpc::RpcBuilder;
use base_consensus_safedb::{DisabledSafeDB, SafeDBReader};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    AlloyL1BlockFetcher, BlockStream, DelegateL2Client, DelegateL2DerivationActor, EngineActor,
    EngineActorRequest, EngineConfig, EngineProcessor, EngineRpcProcessor, L1Config,
    L1WatcherActor, L1WatcherQueryProcessor, NodeActor, QueuedDerivationEngineClient,
    QueuedEngineDerivationClient, QueuedEngineRpcClient, QueuedL1WatcherDerivationClient, RpcActor,
    RpcContext, service::node::HEAD_STREAM_POLL_INTERVAL,
};

/// A lightweight node that follows another L2 node by polling its execution
/// layer RPC and driving the local engine via `NewPayload` + FCU.
///
/// Runs only the [`EngineActor`] and [`DelegateL2DerivationActor`] — no derivation
/// pipeline, P2P, or sequencer.
#[derive(Debug)]
pub struct FollowNode {
    config: Arc<RollupConfig>,
    engine_config: EngineConfig,
    local_l2_provider: RootProvider<Base>,
    l2_source: DelegateL2Client,
    proofs_enabled: bool,
    proofs_max_blocks_ahead: u64,
    l1_config: L1Config,
    rpc_builder: Option<RpcBuilder>,
}

impl FollowNode {
    /// Creates a new [`FollowNode`].
    pub const fn new(
        config: Arc<RollupConfig>,
        engine_config: EngineConfig,
        local_l2_provider: RootProvider<Base>,
        l2_source: DelegateL2Client,
        rpc_builder: Option<RpcBuilder>,
        l1_config: L1Config,
    ) -> Self {
        Self {
            config,
            engine_config,
            local_l2_provider,
            l2_source,
            rpc_builder,
            l1_config,
            proofs_enabled: false,
            proofs_max_blocks_ahead: 512,
        }
    }

    /// Enables proofs sync gating via `debug_proofsSyncStatus`.
    pub const fn with_proofs(mut self, enabled: bool) -> Self {
        self.proofs_enabled = enabled;
        self
    }

    /// Sets the maximum number of blocks the node may advance beyond the
    /// proofs `ExEx` head.
    pub const fn with_proofs_max_blocks_ahead(mut self, max_blocks_ahead: u64) -> Self {
        self.proofs_max_blocks_ahead = max_blocks_ahead;
        self
    }

    fn create_engine_actor<E: EngineClient + 'static>(
        &self,
        engine_client: Arc<E>,
        cancellation_token: CancellationToken,
        engine_request_rx: mpsc::Receiver<EngineActorRequest>,
        derivation_client: QueuedEngineDerivationClient,
    ) -> EngineActor<EngineProcessor<E, QueuedEngineDerivationClient>, EngineRpcProcessor<E>> {
        let engine_state = EngineState::default();
        let (engine_state_tx, engine_state_rx) = watch::channel(engine_state);
        let (engine_queue_length_tx, engine_queue_length_rx) = watch::channel(0);
        let engine = Engine::new(engine_state, engine_state_tx, engine_queue_length_tx);

        let engine_processor = EngineProcessor::new(
            Arc::clone(&engine_client),
            Arc::clone(&self.config),
            derivation_client,
            engine,
            None,
            None,  // no conductor in follow mode
            false, // sequencer_stopped irrelevant for validator/follow mode
        );

        let engine_rpc_processor = EngineRpcProcessor::new(
            Arc::clone(&engine_client),
            Arc::clone(&self.config),
            engine_state_rx,
            engine_queue_length_rx,
        );

        EngineActor::new(
            cancellation_token,
            engine_request_rx,
            engine_processor,
            engine_rpc_processor,
        )
    }

    /// Starts the follow node.
    pub async fn start(&self) -> Result<(), String> {
        let engine_client = Arc::new(
            self.engine_config.clone().build_engine_client().await.map_err(|e| e.to_string())?,
        );
        self.start_inner(engine_client).await
    }

    /// Starts the follow node with a pre-built engine client.
    ///
    /// Enables dependency injection of the engine client for testing scenarios.
    pub async fn start_with_engine_client<E: EngineClient + 'static>(
        &self,
        engine_client: Arc<E>,
    ) -> Result<(), String> {
        self.start_inner(engine_client).await
    }

    async fn start_inner<E: EngineClient + 'static>(
        &self,
        engine_client: Arc<E>,
    ) -> Result<(), String> {
        let cancellation = CancellationToken::new();

        let (derivation_actor_request_tx, derivation_actor_request_rx) = mpsc::channel(1024);
        let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(1024);

        let engine_actor = self.create_engine_actor(
            engine_client,
            cancellation.clone(),
            engine_actor_request_rx,
            QueuedEngineDerivationClient::new(derivation_actor_request_tx.clone()),
        );

        let derivation = DelegateL2DerivationActor::<_>::new(
            QueuedDerivationEngineClient {
                engine_actor_request_tx: engine_actor_request_tx.clone(),
            },
            engine_actor_request_tx.clone(),
            cancellation.clone(),
            derivation_actor_request_rx,
            self.local_l2_provider.clone(),
            self.l2_source.clone(),
        )
        .with_proofs(self.proofs_enabled)
        .with_proofs_max_blocks_ahead(self.proofs_max_blocks_ahead);

        // Create the RPC server actor if configured.
        let rpc = self.rpc_builder.clone().map(|b| {
            // Follow nodes do not run derivation, so they never produce confirmed safe
            // heads to record. Safe head tracking is disabled; the RPC endpoint returns
            // an error if queried.
            RpcActor::new(
                b,
                QueuedEngineRpcClient::new(engine_actor_request_tx.clone()),
                None::<crate::QueuedSequencerAdminAPIClient>,
                Arc::new(DisabledSafeDB) as Arc<dyn SafeDBReader>,
            )
        });

        let (l1_query_tx, l1_query_rx) = mpsc::channel(1024);

        let head_stream = BlockStream::new_as_stream(
            self.l1_config.engine_provider.clone(),
            BlockNumberOrTag::Latest,
            Duration::from_secs(HEAD_STREAM_POLL_INTERVAL),
        )?;
        let finalized_stream = BlockStream::new_as_stream(
            self.l1_config.engine_provider.clone(),
            BlockNumberOrTag::Finalized,
            self.l1_config.finalized_poll_interval,
        )?;

        let (l1_head_updates_tx, _l1_head_updates_rx) = watch::channel(None);
        // Create the [`L1WatcherActor`]. Previously known as the DA watcher actor.
        let l1_watcher = L1WatcherActor::new(
            Arc::clone(&self.config),
            AlloyL1BlockFetcher(self.l1_config.engine_provider.clone()),
            l1_head_updates_tx.clone(),
            QueuedL1WatcherDerivationClient { derivation_actor_request_tx },
            None,
            cancellation.clone(),
            head_stream,
            finalized_stream,
        );
        let l1_query_processor = L1WatcherQueryProcessor::new(
            Arc::clone(&self.config),
            AlloyL1BlockFetcher(self.l1_config.engine_provider.clone()),
            l1_query_rx,
            l1_head_updates_tx.subscribe(),
            cancellation.clone(),
        );

        crate::service::spawn_and_wait!(
            cancellation,
            actors = [
                rpc.map(|r| (
                    r,
                    RpcContext {
                        cancellation: cancellation.clone(),
                        p2p_network: None,
                        network_admin: None,
                        l1_watcher_queries: l1_query_tx,
                    }
                )),
                Some((derivation, ())),
                Some((engine_actor, ())),
                Some((l1_watcher, ())),
                Some((l1_query_processor, ())),
            ]
        );
        Ok(())
    }
}
