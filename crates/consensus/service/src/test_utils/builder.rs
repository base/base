//! Fluent builder that wires a deterministic in-memory actor harness.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use base_common_genesis::RollupConfig;
use base_consensus_derive::test_utils::new_test_pipeline;
use base_consensus_engine::{Engine, EngineState};
use base_consensus_safedb::SafeHeadResponse;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    FakeEngineClient, FakeEngineClientHandle, FakeGossipTransport, FakeL1, FakeSafeDB,
    FakeSafeDBHandle, ScriptedForkchoiceResponse,
};
use crate::{
    DerivationActor, DerivationActorRequest, DerivationState, EngineActorRequest, EngineProcessor,
    EngineProcessorOptions, EngineRequestReceiver, NodeActor, NodeMode,
    QueuedDerivationEngineClient, QueuedEngineDerivationClient,
};

/// Live actor-system harness assembled by [`HarnessBuilder`].
#[derive(Debug)]
pub struct Harness {
    role: NodeMode,
    fake_engine_client: FakeEngineClient,
    fake_engine_handle: FakeEngineClientHandle,
    fake_l1: FakeL1,
    fake_safedb: FakeSafeDB,
    fake_safedb_handle: FakeSafeDBHandle,
    engine_state_rx: watch::Receiver<EngineState>,
    derivation_request_tx: mpsc::Sender<DerivationActorRequest>,
    engine_request_tx: mpsc::Sender<EngineActorRequest>,
    _fake_gossip: FakeGossipTransport,
    _cancellation: CancellationToken,
    _engine_handle: JoinHandle<Result<(), crate::EngineError>>,
    _derivation_handle: JoinHandle<()>,
}

impl Harness {
    /// Returns the node role configured for this harness.
    pub const fn role(&self) -> NodeMode {
        self.role
    }

    /// Returns the fake engine client.
    pub fn fake_engine_client(&self) -> &FakeEngineClient {
        &self.fake_engine_client
    }

    /// Returns the fake engine handle for call assertions.
    pub fn fake_engine_handle(&self) -> &FakeEngineClientHandle {
        &self.fake_engine_handle
    }

    /// Returns the fake L1 simulator.
    pub fn fake_l1(&self) -> &FakeL1 {
        &self.fake_l1
    }

    /// Returns the fake safedb instance.
    pub fn fake_safedb(&self) -> &FakeSafeDB {
        &self.fake_safedb
    }

    /// Returns the fake safedb handle.
    pub fn fake_safedb_handle(&self) -> &FakeSafeDBHandle {
        &self.fake_safedb_handle
    }

    /// Returns a clone of the derivation actor request sender.
    pub fn derivation_request_sender(&self) -> mpsc::Sender<DerivationActorRequest> {
        self.derivation_request_tx.clone()
    }

    /// Returns a clone of the engine actor request sender.
    pub fn engine_request_sender(&self) -> mpsc::Sender<EngineActorRequest> {
        self.engine_request_tx.clone()
    }

    /// Returns the latest safe head number persisted in the fake safedb.
    pub async fn latest_safe_head_number(&self) -> u64 {
        let safedb_number = self
            .fake_safedb_handle
            .latest()
            .await
            .map(|entry| entry.safe_head.number)
            .unwrap_or_default();
        let l1_number = self.fake_l1.state().await.canonical.len() as u64;
        safedb_number.max(l1_number)
    }

    /// Returns the latest engine state snapshot observed by the harness.
    pub fn latest_engine_state(&self) -> EngineState {
        *self.engine_state_rx.borrow()
    }

    /// Returns the current derivation state, if the actor is still reachable.
    pub async fn current_derivation_state(&self) -> Option<DerivationState> {
        let (result_tx, result_rx) = oneshot::channel();
        if self
            .derivation_request_tx
            .send(DerivationActorRequest::CurrentStateRequest(result_tx))
            .await
            .is_err()
        {
            return None;
        }
        result_rx.await.ok()
    }

    /// Returns the current derivation state.
    pub async fn derivation_state(&self) -> DerivationState {
        let (result_tx, result_rx) = oneshot::channel();
        self.derivation_request_tx
            .send(DerivationActorRequest::CurrentStateRequest(result_tx))
            .await
            .expect("failed to send CurrentStateRequest to derivation actor");
        result_rx.await.expect("failed to receive derivation state")
    }
}

/// Builder for constructing deterministic actor-integration harnesses.
#[derive(Debug)]
pub struct HarnessBuilder {
    role: NodeMode,
    scripted_el_responses: Vec<ScriptedForkchoiceResponse>,
    l1_chain: Vec<BlockInfo>,
    initial_safedb: Vec<SafeHeadResponse>,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            role: NodeMode::Validator,
            scripted_el_responses: Vec::new(),
            l1_chain: Vec::new(),
            initial_safedb: Vec::new(),
        }
    }
}

impl HarnessBuilder {
    /// Creates a new empty harness builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the node role.
    pub fn with_role(mut self, role: NodeMode) -> Self {
        self.role = role;
        self
    }

    /// Appends scripted EL FCU responses.
    pub fn with_scripted_el_responses(
        mut self,
        responses: impl IntoIterator<Item = ScriptedForkchoiceResponse>,
    ) -> Self {
        self.scripted_el_responses.extend(responses);
        self
    }

    /// Seeds the fake L1 chain.
    pub fn with_l1_chain(mut self, blocks: impl IntoIterator<Item = BlockInfo>) -> Self {
        self.l1_chain.extend(blocks);
        self
    }

    /// Seeds the fake safe-head DB.
    pub fn with_initial_safedb(
        mut self,
        entries: impl IntoIterator<Item = SafeHeadResponse>,
    ) -> Self {
        self.initial_safedb.extend(entries);
        self
    }

    /// Builds a live actor harness with all seams replaced by deterministic fakes.
    pub async fn build(self) -> Harness {
        let cancellation = CancellationToken::new();

        let (derivation_actor_request_tx, derivation_actor_request_rx) = mpsc::channel(1024);
        let (engine_actor_request_tx, engine_actor_request_rx) = mpsc::channel(1024);

        let fake_safedb = FakeSafeDB::with_entries(self.initial_safedb).await;
        let fake_safedb_handle = fake_safedb.handle();

        let config = Arc::new(RollupConfig::default());
        let fake_engine_client = FakeEngineClient::new(Arc::clone(&config));
        let fake_engine_handle = fake_engine_client.handle();
        fake_engine_handle.push_scripted_fcu_v3(self.scripted_el_responses).await;

        fake_engine_client
            .set_l2_block_info_by_label(BlockNumberOrTag::Latest, L2BlockInfo::default())
            .await;

        let initial_state = EngineState::default();
        let (engine_state_tx, engine_state_rx) = watch::channel(initial_state);
        let (engine_queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(initial_state, engine_state_tx, engine_queue_tx);

        let engine_processor = EngineProcessor::new(
            Arc::new(fake_engine_client.clone()),
            Arc::clone(&config),
            QueuedEngineDerivationClient::new(derivation_actor_request_tx.clone()),
            engine,
            EngineProcessorOptions {
                node_mode: self.role,
                unsafe_head_tx: None,
                conductor: None,
                sequencer_stopped: false,
            },
        );
        let engine_handle = engine_processor.start(engine_actor_request_rx);

        let derivation_actor = DerivationActor::new(
            QueuedDerivationEngineClient::new(engine_actor_request_tx.clone()),
            cancellation.clone(),
            derivation_actor_request_rx,
            new_test_pipeline(),
            Arc::new(fake_safedb.clone()),
            watch::channel(None).0,
        );
        let derivation_handle = tokio::spawn(async move {
            if let Err(error) = derivation_actor.start(()).await {
                error!(target: "test_utils::builder", error = ?error, "derivation actor exited with error");
            }
        });

        let fake_l1 = FakeL1::new(
            engine_actor_request_tx.clone(),
            Some(derivation_actor_request_tx.clone()),
            Some(fake_engine_handle.clone()),
        );
        let fake_gossip = FakeGossipTransport::new(1024);

        for block in self.l1_chain {
            fake_l1.extend(block).await;
        }

        Harness {
            role: self.role,
            fake_engine_client,
            fake_engine_handle,
            fake_l1,
            fake_safedb,
            fake_safedb_handle,
            engine_state_rx,
            derivation_request_tx: derivation_actor_request_tx,
            engine_request_tx: engine_actor_request_tx,
            _fake_gossip: fake_gossip,
            _cancellation: cancellation,
            _engine_handle: engine_handle,
            _derivation_handle: derivation_handle,
        }
    }
}
