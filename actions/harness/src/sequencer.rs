use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_eips::{eip2718::Encodable2718, eip7685::EMPTY_REQUESTS_HASH};
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, B256, Bytes, Signature, U256};
use alloy_rpc_types_engine::{CancunPayloadFields, PayloadId, PraguePayloadFields};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    BasePayloadAttributes, NetworkPayloadEnvelope, PayloadHash,
};
use base_consensus_derive::{AttributesBuilder, PipelineResult, StatefulAttributesBuilder};
use base_consensus_node::{
    Conductor, ConductorError, L1OriginSelector, NodeActor, OriginSelector, PayloadBuilder,
    RecoveryModeGuard, SequencerActor, SequencerActorError, SequencerEngineClient,
    UnsafePayloadGossipClient, UnsafePayloadGossipClientError,
};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    ActionEngineClient, ActionL1ChainProvider, ActionL2ChainProvider, SharedBlockHashRegistry,
    SharedL1Chain, SupervisedP2P, TEST_ACCOUNT_KEY, TestAccount,
};

/// Error type returned by [`L2Sequencer`].
#[derive(Debug, thiserror::Error)]
pub enum L2SequencerError {
    /// The L1 block required for the current epoch is missing from the chain.
    #[error("L1 block {0} not found in shared chain")]
    MissingL1Block(u64),
    /// Failed to build the L1 info deposit transaction.
    #[error("failed to build L1 info deposit: {0}")]
    L1Info(#[from] base_protocol::BlockInfoError),
    /// Transaction signing failed.
    #[error("signing failed: {0}")]
    Signing(#[from] alloy_signer::Error),
    /// EVM execution failed.
    #[error("EVM execution failed: {0}")]
    Evm(String),
    /// Origin selection failed.
    #[error("origin selection failed: {0}")]
    OriginSelection(String),
    /// Attributes construction failed.
    #[error("attributes construction failed: {0}")]
    Attributes(String),
    /// Engine client error.
    #[error("engine client error: {0}")]
    Engine(String),
    /// Payload conversion error.
    #[error("payload conversion error: {0}")]
    PayloadConversion(String),
    /// Conductor rejected the block (e.g. not leader, RPC error).
    #[error("conductor error: {0}")]
    Conductor(#[from] ConductorError),
    /// This sequencer is not the conductor leader and cannot build blocks.
    #[error("sequencer is not the conductor leader")]
    NotLeader,
    /// The production sequencer actor failed.
    #[error("sequencer actor error: {0}")]
    Actor(String),
    /// The production sequencer actor did not insert a block before the timeout.
    #[error("sequencer actor timed out waiting for inserted block")]
    Timeout,
    /// The inserted-block notification channel closed before a block was produced.
    #[error("sequencer actor exited before inserting a block")]
    InsertChannelClosed,
}

/// Converts between execution payload envelopes and action-harness block/gossip types.
#[derive(Debug)]
pub struct ExecutionPayloadConverter;

impl ExecutionPayloadConverter {
    /// Convert a sealed execution payload envelope into a [`BaseBlock`].
    pub fn block_from_envelope(
        envelope: &BaseExecutionPayloadEnvelope,
    ) -> Result<BaseBlock, L2SequencerError> {
        let pbbr = envelope.parent_beacon_block_root;
        let sidecar = match &envelope.execution_payload {
            BaseExecutionPayload::V4(_) => BaseExecutionPayloadSidecar::v4(
                CancunPayloadFields {
                    parent_beacon_block_root: pbbr.unwrap_or_default(),
                    versioned_hashes: vec![],
                },
                PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
            ),
            _ => pbbr.map_or_else(BaseExecutionPayloadSidecar::default, |pbbr| {
                BaseExecutionPayloadSidecar::v3(CancunPayloadFields {
                    parent_beacon_block_root: pbbr,
                    versioned_hashes: vec![],
                })
            }),
        };
        envelope
            .execution_payload
            .clone()
            .try_into_block_with_sidecar(&sidecar)
            .map_err(|e| L2SequencerError::PayloadConversion(format!("{e}")))
    }

    /// Convert a [`BaseBlock`] into a gossip network envelope, signing when a key is supplied.
    pub fn network_envelope(
        block: &BaseBlock,
        signer: Option<&PrivateKeySigner>,
        chain_id: u64,
    ) -> NetworkPayloadEnvelope {
        let block_hash = block.header.hash_slow();
        let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(block_hash, block);
        let parent_beacon_block_root = block.header.parent_beacon_block_root;

        let (signature, payload_hash) = signer.map_or_else(
            || (Signature::new(U256::ZERO, U256::ZERO, false), PayloadHash(B256::ZERO)),
            |signer| {
                let envelope = BaseExecutionPayloadEnvelope {
                    execution_payload: execution_payload.clone(),
                    parent_beacon_block_root,
                };
                let ph = envelope.payload_hash();
                let msg = ph.signature_message(chain_id);
                let sig = signer.sign_hash_sync(&msg).expect("unsafe block signing must not fail");
                (sig, ph)
            },
        );

        NetworkPayloadEnvelope {
            payload: execution_payload,
            signature,
            payload_hash,
            parent_beacon_block_root,
        }
    }
}

/// Attributes builder adapter that injects one test-controlled transaction batch.
#[derive(Debug)]
pub struct ActionSequencerAttributesBuilder {
    inner: StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    user_txs: Vec<BaseTxEnvelope>,
}

impl ActionSequencerAttributesBuilder {
    /// Create a new attributes adapter.
    pub const fn new(
        inner: StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
        user_txs: Vec<BaseTxEnvelope>,
    ) -> Self {
        Self { inner, user_txs }
    }
}

#[async_trait]
impl AttributesBuilder for ActionSequencerAttributesBuilder {
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: alloy_eips::BlockNumHash,
    ) -> PipelineResult<BasePayloadAttributes> {
        let mut attrs = self.inner.prepare_payload_attributes(l2_parent, epoch).await?;
        let encoded_user_txs: Vec<Bytes> = std::mem::take(&mut self.user_txs)
            .into_iter()
            .map(|tx| {
                let mut buf = Vec::new();
                tx.encode_2718(&mut buf);
                Bytes::from(buf)
            })
            .collect();
        if !encoded_user_txs.is_empty() {
            attrs.transactions.get_or_insert_with(Vec::new).extend(encoded_user_txs);
        }
        attrs.no_tx_pool = Some(true);
        Ok(attrs)
    }
}

/// L1 origin selector adapter that supports test-controlled origin pinning.
#[derive(Debug)]
pub struct ActionOriginSelector {
    inner: L1OriginSelector<SharedL1Chain>,
    pin: Option<BlockInfo>,
}

impl ActionOriginSelector {
    /// Create a new origin selector adapter.
    pub const fn new(inner: L1OriginSelector<SharedL1Chain>, pin: Option<BlockInfo>) -> Self {
        Self { inner, pin }
    }
}

#[async_trait]
impl OriginSelector for ActionOriginSelector {
    async fn next_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
        is_recovery_mode: bool,
    ) -> Result<BlockInfo, base_consensus_node::L1OriginSelectorError> {
        if let Some(pin) = self.pin {
            return Ok(pin);
        }
        self.inner.next_l1_origin(unsafe_head, is_recovery_mode).await
    }
}

/// Conductor adapter that allows the actor to own a cloneable conductor handle.
#[derive(Debug, Clone)]
pub struct ActionConductor {
    inner: Arc<dyn Conductor>,
}

impl ActionConductor {
    /// Create a new conductor adapter.
    pub fn new(inner: Arc<dyn Conductor>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Conductor for ActionConductor {
    async fn leader(&self) -> Result<bool, ConductorError> {
        self.inner.leader().await
    }

    async fn active(&self) -> Result<bool, ConductorError> {
        self.inner.active().await
    }

    async fn commit_unsafe_payload(
        &self,
        payload: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), ConductorError> {
        self.inner.commit_unsafe_payload(payload).await
    }

    async fn override_leader(&self) -> Result<(), ConductorError> {
        self.inner.override_leader().await
    }
}

/// Sequencer engine client adapter that reports inserted blocks back to the harness driver.
#[derive(Debug, Clone)]
pub struct ActionSequencerEngineClient {
    inner: Arc<ActionEngineClient>,
    inserted_tx: mpsc::Sender<(BaseBlock, L2BlockInfo)>,
}

impl ActionSequencerEngineClient {
    /// Create a new engine client adapter.
    pub const fn new(
        inner: Arc<ActionEngineClient>,
        inserted_tx: mpsc::Sender<(BaseBlock, L2BlockInfo)>,
    ) -> Self {
        Self { inner, inserted_tx }
    }
}

#[async_trait]
impl SequencerEngineClient for ActionSequencerEngineClient {
    async fn reset_engine_forkchoice(&self) -> Result<(), base_consensus_node::EngineClientError> {
        self.inner.reset_engine_forkchoice().await
    }

    async fn start_build_block(
        &self,
        attributes: AttributesWithParent,
    ) -> Result<PayloadId, base_consensus_node::EngineClientError> {
        self.inner.start_build_block(attributes).await
    }

    async fn get_sealed_payload(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> Result<BaseExecutionPayloadEnvelope, base_consensus_node::EngineClientError> {
        self.inner.get_sealed_payload(payload_id, attributes).await
    }

    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<L2BlockInfo, base_consensus_node::EngineClientError> {
        let block = ExecutionPayloadConverter::block_from_envelope(&payload)
            .map_err(|e| base_consensus_node::EngineClientError::ResponseError(e.to_string()))?;
        let inserted_head = self.inner.insert_unsafe_payload(payload).await?;
        let _ = self.inserted_tx.send((block, inserted_head)).await;
        Ok(inserted_head)
    }

    async fn get_unsafe_head(&self) -> Result<L2BlockInfo, base_consensus_node::EngineClientError> {
        self.inner.get_unsafe_head().await
    }
}

/// No-op gossip adapter used by the actor; tests still inject gossip explicitly.
#[derive(Debug, Clone, Default)]
pub struct ActionUnsafePayloadGossipClient;

#[async_trait]
impl UnsafePayloadGossipClient for ActionUnsafePayloadGossipClient {
    async fn schedule_execution_payload_gossip(
        &self,
        _payload: BaseExecutionPayloadEnvelope,
    ) -> Result<(), UnsafePayloadGossipClientError> {
        Ok(())
    }
}

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
    l1_origin_pin: Option<BlockInfo>,
    conductor: Option<Arc<dyn Conductor>>,
    unsafe_block_signer: Option<PrivateKeySigner>,
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
            l1_origin_pin: None,
            conductor: None,
            unsafe_block_signer: None,
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
    pub fn storage_at(
        &self,
        address: alloy_primitives::Address,
        slot: alloy_primitives::U256,
    ) -> alloy_primitives::U256 {
        self.engine_client.storage_at(address, slot)
    }

    /// Check whether an account has non-empty code deployed via the engine client.
    pub fn has_code(&self, address: alloy_primitives::Address) -> bool {
        self.engine_client.has_code(address)
    }

    /// Pin the L1 origin to the given block, bypassing automatic epoch advance.
    pub const fn pin_l1_origin(&mut self, origin: BlockInfo) {
        self.l1_origin_pin = Some(origin);
    }

    /// Clear the pinned L1 origin, restoring automatic epoch selection.
    pub const fn clear_l1_origin_pin(&mut self) {
        self.l1_origin_pin = None;
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
        self.conductor = Some(conductor);
    }

    /// Broadcast `block` as a [`NetworkPayloadEnvelope`] to the wired [`SupervisedP2P`] handle.
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
        let conductor = self.conductor.as_ref().map(|c| ActionConductor::new(Arc::clone(c)));
        if let Some(conductor) = &conductor
            && !conductor.leader().await?
        {
            return Err(L2SequencerError::NotLeader);
        }

        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&self.rollup_config),
            Arc::clone(&self.l1_chain_config),
            self.l2_provider.clone(),
            ActionL1ChainProvider::new(self.l1_chain.clone()),
        );
        let attrs_builder = ActionSequencerAttributesBuilder::new(attrs_builder, user_txs);
        let origin_selector =
            L1OriginSelector::new(Arc::clone(&self.rollup_config), self.l1_chain.clone());
        let origin_selector = ActionOriginSelector::new(origin_selector, self.l1_origin_pin);

        // The production ticker cannot be constructed with a zero period, but
        // some action tests intentionally use `RollupConfig::default()`. Keep
        // the real config for attributes/origin selection and clamp only the
        // actor scheduler's private copy.
        let actor_rollup_config = if self.rollup_config.block_time == 0 {
            let mut config = (*self.rollup_config).clone();
            config.block_time = 1;
            Arc::new(config)
        } else {
            Arc::clone(&self.rollup_config)
        };

        let (inserted_tx, mut inserted_rx) = mpsc::channel(1);
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

        let (_admin_api_tx, admin_api_rx) = mpsc::channel(1);
        let cancellation_token = CancellationToken::new();
        let actor = SequencerActor {
            admin_api_rx,
            builder,
            cancellation_token: cancellation_token.clone(),
            conductor,
            engine_client,
            is_active: true,
            recovery_mode: RecoveryModeGuard::new(false),
            rollup_config: actor_rollup_config,
            unsafe_payload_gossip_client: ActionUnsafePayloadGossipClient,
            sealer: None,
            pending_stop: None,
        };

        let mut actor_task = tokio::spawn(async move { actor.start(()).await });
        let sleep = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(sleep);

        let (block, inserted_head) = tokio::select! {
            biased;
            inserted = inserted_rx.recv() => {
                inserted.ok_or(L2SequencerError::InsertChannelClosed)?
            }
            joined = &mut actor_task => {
                return Err(Self::actor_join_error(joined));
            }
            _ = &mut sleep => return Err(L2SequencerError::Timeout),
        };

        cancellation_token.cancel();
        let _ = actor_task.await;

        self.head = inserted_head;
        self.l2_provider.insert_block(inserted_head);
        self.l2_provider.insert_base_block(inserted_head.block_info.number, block.clone());

        Ok(block)
    }

    /// Convert an actor task join result into [`L2SequencerError`].
    pub fn actor_join_error(
        joined: Result<Result<(), SequencerActorError>, tokio::task::JoinError>,
    ) -> L2SequencerError {
        match joined {
            Ok(Ok(())) => L2SequencerError::InsertChannelClosed,
            Ok(Err(err)) => L2SequencerError::Actor(err.to_string()),
            Err(err) => L2SequencerError::Actor(err.to_string()),
        }
    }
}
