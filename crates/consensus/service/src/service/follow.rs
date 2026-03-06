use std::sync::Arc;

use alloy_provider::RootProvider;
use base_alloy_network::Base;
use base_consensus_engine::{Engine, EngineState};
use base_consensus_genesis::RollupConfig;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    DelegateL2Client, DelegateL2DerivationActor, EngineActor, EngineActorRequest, EngineConfig,
    EngineProcessor, EngineRpcProcessor, NodeActor, QueuedDerivationEngineClient,
    QueuedEngineDerivationClient,
};

use super::LocalEngineActor;

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
}

impl FollowNode {
    /// Creates a new [`FollowNode`].
    pub const fn new(
        config: Arc<RollupConfig>,
        engine_config: EngineConfig,
        local_l2_provider: RootProvider<Base>,
        l2_source: DelegateL2Client,
    ) -> Self {
        Self { config, engine_config, local_l2_provider, l2_source }
    }

    fn create_engine_actor(
        &self,
        cancellation_token: CancellationToken,
        engine_request_rx: mpsc::Receiver<EngineActorRequest>,
        derivation_client: QueuedEngineDerivationClient,
    ) -> LocalEngineActor {
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
        );

        crate::service::spawn_and_wait!(
            cancellation,
            actors = [Some((derivation, ())), Some((engine_actor, ())),]
        );
        Ok(())
    }
}
