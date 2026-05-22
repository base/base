use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use base_common_genesis::RollupConfig;
use base_consensus_derive::StatefulAttributesBuilder;
use base_consensus_node::{
    Conductor, L1OriginSelector, NodeActor, PayloadBuilder, RecoveryModeGuard, SequencerActor,
    SequencerActorError, SequencerAdminQuery,
};
use base_consensus_rpc::SequencerAdminAPIError;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use super::{
    ActionConductor, ActionOriginSelector, ActionSequencerAttributesBuilder,
    ActionSequencerEngineClient, ActionUnsafePayloadGossipClient, ExecutionPayloadConverter,
    L2SequencerError,
};
use crate::{
    ActionEngineClient, ActionL1ChainProvider, ActionL2ChainProvider, SharedBlockHashRegistry,
    SharedL1Chain, SupervisedP2P, TEST_ACCOUNT_KEY, TestAccount,
};

/// Builds real [`BaseBlock`]s for use in action tests using the production sequencer actor.
#[derive(Debug)]
pub struct L2Sequencer {
    head: L2BlockInfo,
    engine_client: Arc<ActionEngineClient>,
    rollup_config: Arc<RollupConfig>,
    l1_chain_config: Arc<ChainConfig>,
    l1_chain: SharedL1Chain,
    l2_provider: ActionL2ChainProvider,
    test_account: Arc<Mutex<TestAccount>>,
    block_hashes: SharedBlockHashRegistry,
    supervised_p2p: Option<SupervisedP2P>,
    l1_origin_pin: Arc<Mutex<Option<BlockInfo>>>,
    conductor: Arc<Mutex<Option<Arc<dyn Conductor>>>>,
    unsafe_block_signer: Option<PrivateKeySigner>,
    user_txs: Arc<Mutex<Option<Vec<BaseTxEnvelope>>>>,
    admin_api_tx: Option<mpsc::Sender<SequencerAdminQuery>>,
    inserted_rx: Option<mpsc::Receiver<(BaseBlock, L2BlockInfo)>>,
    cancellation_token: Option<CancellationToken>,
    actor_task: Option<JoinHandle<Result<(), SequencerActorError>>>,
}

impl L2Sequencer {
    /// Create a new sequencer using the production [`SequencerActor`].
    pub fn new(
        head: L2BlockInfo,
        engine_client: Arc<ActionEngineClient>,
        rollup_config: Arc<RollupConfig>,
        l1_chain_config: Arc<ChainConfig>,
        l1_chain: SharedL1Chain,
        l2_provider: ActionL2ChainProvider,
    ) -> Self {
        let test_account = Arc::new(Mutex::new(TestAccount::new(TEST_ACCOUNT_KEY)));
        let block_hashes = engine_client.block_hash_registry();

        Self {
            head,
            engine_client,
            rollup_config,
            l1_chain_config,
            l1_chain,
            l2_provider,
            test_account,
            block_hashes,
            supervised_p2p: None,
            l1_origin_pin: Arc::new(Mutex::new(None)),
            conductor: Arc::new(Mutex::new(None)),
            unsafe_block_signer: None,
            user_txs: Arc::new(Mutex::new(None)),
            admin_api_tx: None,
            inserted_rx: None,
            cancellation_token: None,
            actor_task: None,
        }
    }

    /// Return the current unsafe L2 head.
    pub const fn head(&self) -> L2BlockInfo {
        self.head
    }

    /// Return a shared handle to the sequencer's test account.
    pub fn test_account(&self) -> Arc<Mutex<TestAccount>> {
        Arc::clone(&self.test_account)
    }

    /// Return the sequencer's shared block-hash registry.
    pub fn block_hash_registry(&self) -> SharedBlockHashRegistry {
        self.block_hashes.clone()
    }

    /// Return a clone of the sequencer's engine client.
    pub fn engine_client(&self) -> Arc<ActionEngineClient> {
        Arc::clone(&self.engine_client)
    }

    /// Read a storage value from the latest committed state via the engine client.
    pub fn storage_at(&self, address: Address, slot: U256) -> U256 {
        self.engine_client.storage_at(address, slot)
    }

    /// Check whether an account has non-empty code deployed via the engine client.
    pub fn has_code(&self, address: Address) -> bool {
        self.engine_client.has_code(address)
    }

    /// Return receipts for an executed block number.
    pub fn receipts_at(&self, block_number: u64) -> Option<Vec<BaseReceipt>> {
        self.engine_client.receipts_at(block_number)
    }

    /// Pin the L1 origin to the given block, bypassing automatic epoch advance.
    pub fn pin_l1_origin(&mut self, origin: BlockInfo) {
        *self.l1_origin_pin.lock().expect("L1 origin pin lock poisoned") = Some(origin);
    }

    /// Clear the pinned L1 origin, restoring automatic epoch selection.
    pub fn clear_l1_origin_pin(&mut self) {
        *self.l1_origin_pin.lock().expect("L1 origin pin lock poisoned") = None;
    }

    /// Wire a [`SupervisedP2P`] handle to this sequencer for explicit gossip injection.
    pub fn set_supervised_p2p(&mut self, p2p: SupervisedP2P) {
        self.supervised_p2p = Some(p2p);
    }

    /// Attach an unsafe block signing key to this sequencer.
    pub fn set_unsafe_block_signer(&mut self, key: PrivateKeySigner) {
        self.unsafe_block_signer = Some(key);
    }

    /// Return the address corresponding to the configured unsafe block signing key, if any.
    pub fn unsafe_block_signer_address(&self) -> Option<Address> {
        self.unsafe_block_signer.as_ref().map(|s| s.address())
    }

    /// Attach a conductor to this sequencer.
    pub fn set_conductor(&mut self, conductor: Arc<dyn Conductor>) {
        *self.conductor.lock().expect("conductor lock poisoned") = Some(conductor);
    }

    /// Broadcast `block` as a [`base_common_rpc_types_engine::NetworkPayloadEnvelope`] to the wired [`SupervisedP2P`] handle.
    pub fn broadcast_unsafe_block(&self, block: &BaseBlock) {
        let Some(p2p) = &self.supervised_p2p else { return };
        p2p.send(ExecutionPayloadConverter::network_envelope(
            block,
            self.unsafe_block_signer.as_ref(),
            self.rollup_config.l2_chain_id.id(),
        ));
    }

    /// Build the next L2 block containing no user transactions.
    pub async fn build_empty_block(&mut self) -> BaseBlock {
        self.build_next_block_with_transactions(vec![]).await
    }

    /// Build the next L2 block with a single transaction.
    pub async fn build_next_block_with_single_transaction(&mut self) -> BaseBlock {
        let tx = {
            let mut account = self.test_account.lock().expect("test account lock poisoned");
            account.create_eip1559_tx(self.rollup_config.l2_chain_id.id())
        };
        self.build_next_block_with_transactions(vec![tx]).await
    }

    /// Build `count` sequential L2 blocks with one user transaction each.
    pub async fn build_next_blocks_with_single_transactions(
        &mut self,
        count: u64,
    ) -> Vec<BaseBlock> {
        let mut blocks = Vec::with_capacity(count as usize);
        for _ in 0..count {
            blocks.push(self.build_next_block_with_single_transaction().await);
        }
        blocks
    }

    /// Build the next L2 block and advance the internal head.
    pub async fn build_next_block_with_transactions(
        &mut self,
        transactions: Vec<BaseTxEnvelope>,
    ) -> BaseBlock {
        self.try_build_next_block_with_transactions(transactions)
            .await
            .unwrap_or_else(|e| panic!("L2Sequencer::build_next_block failed: {e}"))
    }

    /// Build the next L2 block, returning an error instead of panicking.
    pub async fn try_build_next_block_with_transactions(
        &mut self,
        user_txs: Vec<BaseTxEnvelope>,
    ) -> Result<BaseBlock, L2SequencerError> {
        if !self.conductor_leader().await? {
            return Err(L2SequencerError::NotLeader);
        }

        self.ensure_actor_started().await?;
        self.queue_user_txs(user_txs)?;
        if let Err(err) = self.start_sequencer().await {
            self.clear_queued_user_txs();
            return Err(err);
        }

        let (block, inserted_head) = match self.wait_for_inserted_block().await {
            Ok(inserted) => inserted,
            Err(err) => {
                self.clear_queued_user_txs();
                let _ = self.stop_sequencer(self.head.block_info.hash).await;
                return Err(err);
            }
        };

        self.head = inserted_head;
        self.l2_provider.insert_block(inserted_head);
        self.l2_provider.insert_base_block(inserted_head.block_info.number, block.clone());
        self.stop_sequencer(inserted_head.block_info.hash).await?;

        Ok(block)
    }

    /// Start the production actor task if it has not been started yet.
    pub async fn ensure_actor_started(&mut self) -> Result<(), L2SequencerError> {
        if let Some(actor_task) = &self.actor_task {
            if actor_task.is_finished() {
                let actor_task = self.actor_task.take().expect("actor task checked above");
                return Err(Self::actor_join_error(actor_task.await));
            }
            return Ok(());
        }

        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&self.rollup_config),
            Arc::clone(&self.l1_chain_config),
            self.l2_provider.clone(),
            ActionL1ChainProvider::new(self.l1_chain.clone()),
        );
        let attrs_builder =
            ActionSequencerAttributesBuilder::new(attrs_builder, Arc::clone(&self.user_txs));
        let origin_selector =
            L1OriginSelector::new(Arc::clone(&self.rollup_config), self.l1_chain.clone());
        let origin_selector =
            ActionOriginSelector::new(origin_selector, Arc::clone(&self.l1_origin_pin));

        let (inserted_tx, inserted_rx) = mpsc::channel(8);
        let engine_client = Arc::new(ActionSequencerEngineClient::new(
            Arc::clone(&self.engine_client),
            inserted_tx,
        ));
        let builder = PayloadBuilder {
            attributes_builder: attrs_builder,
            engine_client: Arc::clone(&engine_client),
            origin_selector,
            recovery_mode: RecoveryModeGuard::new(false),
            rollup_config: Arc::clone(&self.rollup_config),
        };

        let (admin_api_tx, admin_api_rx) = mpsc::channel(8);
        let cancellation_token = CancellationToken::new();
        let actor = SequencerActor {
            admin_api_rx,
            builder,
            cancellation_token: cancellation_token.clone(),
            conductor: Some(ActionConductor::new(Arc::clone(&self.conductor))),
            engine_client,
            is_active: false,
            recovery_mode: RecoveryModeGuard::new(false),
            rollup_config: self.actor_rollup_config(),
            unsafe_payload_gossip_client: ActionUnsafePayloadGossipClient,
            sealer: None,
            pending_stop: None,
        };

        self.admin_api_tx = Some(admin_api_tx);
        self.inserted_rx = Some(inserted_rx);
        self.cancellation_token = Some(cancellation_token);
        self.actor_task = Some(tokio::spawn(async move { actor.start(()).await }));
        Ok(())
    }

    /// Return a rollup config suitable for the actor scheduler.
    pub fn actor_rollup_config(&self) -> Arc<RollupConfig> {
        // Action tests explicitly ask the actor for one block at a time. Keep
        // the real config for attributes/origin selection and use a short
        // cadence only for the actor scheduler's private copy, so tests with
        // large L2 block times do not wait for wall-clock production slots.
        if self.rollup_config.block_time != 1 {
            let mut config = (*self.rollup_config).clone();
            config.block_time = 1;
            Arc::new(config)
        } else {
            Arc::clone(&self.rollup_config)
        }
    }

    /// Return true when this sequencer can act as conductor leader.
    pub async fn conductor_leader(&self) -> Result<bool, L2SequencerError> {
        let conductor = self.conductor.lock().expect("conductor lock poisoned").clone();
        match conductor {
            Some(conductor) => Ok(conductor.leader().await?),
            None => Ok(true),
        }
    }

    /// Queue the next harness-controlled transaction batch for the actor.
    pub fn queue_user_txs(&self, user_txs: Vec<BaseTxEnvelope>) -> Result<(), L2SequencerError> {
        let mut queued = self.user_txs.lock().expect("sequencer user tx queue lock poisoned");
        if queued.is_some() {
            return Err(L2SequencerError::Admin(
                "sequencer already has a queued transaction batch".to_string(),
            ));
        }
        *queued = Some(user_txs);
        Ok(())
    }

    /// Clear any queued transaction batch.
    pub fn clear_queued_user_txs(&self) {
        *self.user_txs.lock().expect("sequencer user tx queue lock poisoned") = None;
    }

    /// Ask the production actor to start sequencing from the current head.
    pub async fn start_sequencer(&self) -> Result<(), L2SequencerError> {
        let (tx, rx) = oneshot::channel();
        self.admin_api_tx()?
            .send(SequencerAdminQuery::StartSequencer(self.head.block_info.hash, tx))
            .await
            .map_err(|_| L2SequencerError::Admin("sequencer admin channel closed".to_string()))?;
        match rx.await.map_err(|_| {
            L2SequencerError::Admin("sequencer start response channel closed".to_string())
        })? {
            Ok(()) => Ok(()),
            Err(SequencerAdminAPIError::NotLeader) => Err(L2SequencerError::NotLeader),
            Err(err) => Err(L2SequencerError::Admin(err.to_string())),
        }
    }

    /// Ask the production actor to stop sequencing after the requested block is inserted.
    pub async fn stop_sequencer(&self, expected_head: B256) -> Result<(), L2SequencerError> {
        let (tx, rx) = oneshot::channel();
        self.admin_api_tx()?
            .send(SequencerAdminQuery::StopSequencer(tx))
            .await
            .map_err(|_| L2SequencerError::Admin("sequencer admin channel closed".to_string()))?;
        let stopped_head = rx
            .await
            .map_err(|_| {
                L2SequencerError::Admin("sequencer stop response channel closed".to_string())
            })?
            .map_err(|err| L2SequencerError::Admin(err.to_string()))?;
        if stopped_head != expected_head {
            return Err(L2SequencerError::Admin(format!(
                "sequencer stopped at {stopped_head}, expected {expected_head}",
            )));
        }
        Ok(())
    }

    /// Wait for the actor to insert one block.
    pub async fn wait_for_inserted_block(
        &mut self,
    ) -> Result<(BaseBlock, L2BlockInfo), L2SequencerError> {
        let inserted_rx = self.inserted_rx.as_mut().ok_or_else(|| {
            L2SequencerError::Admin("sequencer inserted-block channel not initialized".to_string())
        })?;
        let sleep = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(sleep);

        tokio::select! {
            biased;
            inserted = inserted_rx.recv() => {
                inserted.ok_or(L2SequencerError::InsertChannelClosed)
            }
            _ = &mut sleep => Err(L2SequencerError::Timeout),
        }
    }

    /// Return the actor admin channel.
    pub fn admin_api_tx(&self) -> Result<&mpsc::Sender<SequencerAdminQuery>, L2SequencerError> {
        self.admin_api_tx.as_ref().ok_or_else(|| {
            L2SequencerError::Admin("sequencer admin channel not initialized".to_string())
        })
    }

    /// Convert an actor task join result into [`L2SequencerError`].
    pub fn actor_join_error(
        joined: Result<Result<(), SequencerActorError>, JoinError>,
    ) -> L2SequencerError {
        match joined {
            Ok(Ok(())) => L2SequencerError::InsertChannelClosed,
            Ok(Err(err)) => L2SequencerError::Actor(err.to_string()),
            Err(err) => L2SequencerError::Actor(err.to_string()),
        }
    }
}

impl Drop for L2Sequencer {
    fn drop(&mut self) {
        if let Some(cancellation_token) = &self.cancellation_token {
            cancellation_token.cancel();
        }
        if let Some(actor_task) = &self.actor_task {
            actor_task.abort();
        }
    }
}
