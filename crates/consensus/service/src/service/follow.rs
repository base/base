use std::sync::Arc;

use alloy_provider::RootProvider;
use base_alloy_network::Base;
use base_consensus_engine::{Engine, EngineState, OpEngineClient};
use base_consensus_genesis::RollupConfig;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    DelegateL2Client, DelegateL2DerivationActor, EngineActor, EngineActorRequest, EngineConfig,
    EngineProcessor, EngineRpcProcessor, NodeActor, QueuedDerivationEngineClient,
    QueuedEngineDerivationClient,
};

/// A lightweight node that follows another L2 node by polling its execution
/// layer RPC and driving the local engine via `NewPayload` + FCU.
///
/// Runs only the [`EngineActor`] and [`DelegateL2DerivationActor`] — no L1
/// watcher, derivation pipeline, P2P, or sequencer.
#[derive(Debug)]
pub struct FollowNode {
    config: Arc<RollupConfig>,
    engine_config: EngineConfig,
    local_l2_provider: RootProvider<Base>,
    l2_source: DelegateL2Client,
    proofs_enabled: bool,
    proofs_max_blocks_ahead: u64,
}

impl FollowNode {
    /// Creates a new [`FollowNode`].
    pub const fn new(
        config: Arc<RollupConfig>,
        engine_config: EngineConfig,
        local_l2_provider: RootProvider<Base>,
        l2_source: DelegateL2Client,
    ) -> Self {
        Self {
            config,
            engine_config,
            local_l2_provider,
            l2_source,
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
    /// proofs ExEx head.
    pub const fn with_proofs_max_blocks_ahead(mut self, max_blocks_ahead: u64) -> Self {
        self.proofs_max_blocks_ahead = max_blocks_ahead;
        self
    }

    #[allow(clippy::type_complexity)]
    fn create_engine_actor(
        &self,
        cancellation_token: CancellationToken,
        engine_request_rx: mpsc::Receiver<EngineActorRequest>,
        derivation_client: QueuedEngineDerivationClient,
    ) -> EngineActor<
        EngineProcessor<
            OpEngineClient<RootProvider, RootProvider<Base>>,
            QueuedEngineDerivationClient,
        >,
        EngineRpcProcessor<OpEngineClient<RootProvider, RootProvider<Base>>>,
    > {
        let engine_state = EngineState::default();
        let (engine_state_tx, engine_state_rx) = watch::channel(engine_state);
        let (engine_queue_length_tx, engine_queue_length_rx) = watch::channel(0);
        let engine = Engine::new(engine_state, engine_state_tx, engine_queue_length_tx);

        let engine_client = Arc::new(self.engine_config.clone().build_engine_client());

        let engine_processor = EngineProcessor::new(
            Arc::clone(&engine_client),
            Arc::clone(&self.config),
            derivation_client,
            engine,
            None,
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
        let cancellation = CancellationToken::new();

        let (derivation_actor_request_tx, derivation_actor_request_rx) = mpsc::channel(1024);
        let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(1024);

        let engine_actor = self.create_engine_actor(
            cancellation.clone(),
            engine_actor_request_rx,
            QueuedEngineDerivationClient::new(derivation_actor_request_tx),
        );

        let derivation = DelegateL2DerivationActor::<_>::new(
            QueuedDerivationEngineClient {
                engine_actor_request_tx: engine_actor_request_tx.clone(),
            },
            engine_actor_request_tx,
            cancellation.clone(),
            derivation_actor_request_rx,
            self.local_l2_provider.clone(),
            self.l2_source.clone(),
        )
        .with_proofs(self.proofs_enabled)
        .with_proofs_max_blocks_ahead(self.proofs_max_blocks_ahead);

        crate::service::spawn_and_wait!(
            cancellation,
            actors = [Some((derivation, ())), Some((engine_actor, ())),]
        );
        Ok(())
    }
}
