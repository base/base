use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use alloy_consensus::SignableTransaction;
use alloy_eips::{BlockNumHash, eip2718::Encodable2718, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use alloy_rpc_types_engine::{CancunPayloadFields, PraguePayloadFields};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    NetworkPayloadEnvelope, PayloadHash,
};
use base_consensus_derive::{AttributesBuilder, StatefulAttributesBuilder};
use base_consensus_node::{
    Conductor, ConductorError, L1OriginSelector, OriginSelector, SequencerEngineClient,
};
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

use crate::{
    ActionEngineClient, ActionL1ChainProvider, ActionL2ChainProvider, L2BlockProvider,
    SharedL1Chain, SupervisedP2P,
};

/// Hardcoded private key for the test account used across all action tests.
///
/// The corresponding address is deterministic: derive it via
/// `PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY).unwrap().address()`.
/// Tests that need to fund the account should include it in the genesis
/// allocation with a sufficient ETH balance.
pub const TEST_ACCOUNT_KEY: B256 = B256::new([0x01u8; 32]);

/// The L2 address derived from [`TEST_ACCOUNT_KEY`].
///
/// Pre-computed so callers can reference it without constructing a signer.
// Address derived from the secp256k1 public key of [0x01; 32].
pub const TEST_ACCOUNT_ADDRESS: Address =
    alloy_primitives::address!("1a642f0E3c3aF545E7AcBD38b07251B3990914F1");

/// A test account with nonce tracking and signing capability.
///
/// Wraps a [`PrivateKeySigner`] with an auto-incrementing nonce so callers
/// can build correctly-sequenced signed transactions without manual bookkeeping.
/// Shared via [`Arc`] so the sequencer and external test code stay in sync.
#[derive(Debug)]
pub struct TestAccount {
    signer: PrivateKeySigner,
    nonce: u64,
}

impl TestAccount {
    /// Create a new test account from a private key with nonce starting at 0.
    pub fn new(key: B256) -> Self {
        let signer = PrivateKeySigner::from_bytes(&key).expect("valid key");
        Self { signer, nonce: 0 }
    }

    /// Return the address derived from this account's private key.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }

    /// Sign a pre-built EIP-1559 transaction without modifying the nonce.
    ///
    /// The caller is responsible for setting the correct nonce in the
    /// transaction fields before calling this method.
    pub fn sign_tx(
        &mut self,
        tx: alloy_consensus::TxEip1559,
    ) -> Result<BaseTxEnvelope, alloy_signer::Error> {
        let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
        Ok(BaseTxEnvelope::Eip1559(tx.into_signed(sig)))
    }

    /// Creates and signs a minimal EIP-1559 transfer, auto-incrementing the nonce.
    pub fn create_eip1559_tx(&mut self, chain_id: u64) -> BaseTxEnvelope {
        self.create_tx(chain_id, TxKind::Call(Address::ZERO), Bytes::new(), U256::from(1), 21_000)
    }

    /// Creates and signs a custom EIP-1559 transaction, auto-incrementing the nonce.
    ///
    /// The caller provides the destination, calldata, value, and gas limit.
    /// Chain-level fields (`chain_id`, `nonce`, fee caps) are filled in automatically.
    pub fn create_tx(
        &mut self,
        chain_id: u64,
        to: TxKind,
        input: Bytes,
        value: U256,
        gas_limit: u64,
    ) -> BaseTxEnvelope {
        let tx = alloy_consensus::TxEip1559 {
            chain_id,
            nonce: self.nonce,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            gas_limit,
            to,
            value,
            input,
            access_list: Default::default(),
        };
        let sig = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("test account signing must not fail");
        self.nonce += 1;
        BaseTxEnvelope::Eip1559(tx.into_signed(sig))
    }

    /// Return the current nonce.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }
}

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
}

/// A pre-built queue of [`BaseBlock`]s for the batcher to drain.
///
/// Tests push fully-formed blocks into the source, which the batcher
/// consumes one at a time via [`L2BlockProvider::next_block`].
#[derive(Debug, Default)]
pub struct ActionL2Source {
    blocks: VecDeque<BaseBlock>,
}

impl ActionL2Source {
    /// Create an empty source.
    pub const fn new() -> Self {
        Self { blocks: VecDeque::new() }
    }

    /// Push a block to the back of the queue.
    pub fn push(&mut self, block: BaseBlock) {
        self.blocks.push_back(block);
    }

    /// Return the number of blocks remaining.
    pub fn remaining(&self) -> usize {
        self.blocks.len()
    }

    /// Return `true` if the source has been fully drained.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl L2BlockProvider for ActionL2Source {
    fn next_block(&mut self) -> Option<BaseBlock> {
        self.blocks.pop_front()
    }
}

/// Underlying map type for [`SharedBlockHashRegistry`]: block number -> (hash, optional state root).
pub type BlockHashInner = Arc<Mutex<HashMap<u64, (B256, Option<B256>)>>>;

/// Shared L2 block hashes and state roots keyed by block number.
///
/// `L2Sequencer` writes into this registry as blocks are built, and
/// `TestRollupNode` reads from the same registry when it applies derived
/// attributes so the resulting safe-head hash chain matches the sequencer's
/// sealed headers. The [`ActionEngineClient`] reads the stored state root for
/// post-derivation execution validation.
///
/// The state root field is `Option<B256>`: it is `Some` only when the entry
/// was produced by real EVM execution (e.g. via [`L2Sequencer`] or
/// [`TestRollupNode::act_l2_unsafe_gossip_receive`]). Entries created with
/// [`TestRollupNode::register_block_hash`] store `None`, which causes the
/// executor to skip state-root validation for that block rather than panic
/// against a bogus sentinel value.
///
/// [`TestRollupNode::act_l2_unsafe_gossip_receive`]: crate::TestRollupNode::act_l2_unsafe_gossip_receive
/// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
#[derive(Debug, Clone, Default)]
pub struct SharedBlockHashRegistry(BlockHashInner);

impl SharedBlockHashRegistry {
    /// Create an empty shared registry.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Record the block hash and optional state root for an L2 block number.
    ///
    /// Pass `Some(state_root)` when the block was produced by real EVM
    /// execution so that the engine client can validate it.
    /// Pass `None` for synthetic blocks (e.g. via
    /// [`TestRollupNode::register_block_hash`]); the executor will skip
    /// state-root validation for those blocks.
    ///
    /// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
    pub fn insert(&self, number: u64, hash: B256, state_root: Option<B256>) {
        self.0
            .lock()
            .expect("block hash registry lock poisoned")
            .insert(number, (hash, state_root));
    }

    /// Return the registered block hash for an L2 block number.
    pub fn get(&self, number: u64) -> Option<B256> {
        self.0.lock().expect("block hash registry lock poisoned").get(&number).map(|(h, _)| *h)
    }

    /// Return the registered state root for an L2 block number, if any.
    ///
    /// Returns `None` when the block was not registered or was registered
    /// without a state root (e.g. via [`TestRollupNode::register_block_hash`]).
    ///
    /// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
    pub fn get_state_root(&self, number: u64) -> Option<B256> {
        self.0.lock().expect("block hash registry lock poisoned").get(&number).and_then(|(_, s)| *s)
    }
}

/// Builds real [`BaseBlock`]s for use in action tests using production components.
///
/// Uses:
/// - [`L1OriginSelector`] for epoch selection (same as the production sequencer)
/// - [`StatefulAttributesBuilder`] for L1-info deposit and attribute construction
/// - [`ActionEngineClient`] via [`SequencerEngineClient`] for block building
///
/// Each block contains:
/// - A correct L1-info deposit transaction (type `0x7E`) as the first
///   transaction, built from the actual L1 block at the current epoch.
/// - A configurable number of signed EIP-1559 user transactions from the
///   test account ([`TEST_ACCOUNT_KEY`]).
///
/// Epoch selection mirrors the real sequencer via [`L1OriginSelector`],
/// unless an L1 origin is pinned via [`pin_l1_origin`].
///
/// [`pin_l1_origin`]: L2Sequencer::pin_l1_origin
#[derive(Debug)]
pub struct L2Sequencer {
    /// Production L1 origin selector.
    origin_selector: L1OriginSelector<SharedL1Chain>,
    /// Production attributes builder.
    attributes_builder: StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
    /// Production engine client for block building.
    engine_client: Arc<ActionEngineClient>,
    /// Current unsafe L2 head.
    head: L2BlockInfo,
    /// Rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// Test account used for signing user transactions.
    test_account: Arc<Mutex<TestAccount>>,
    /// Shared registry of built L2 block hashes, keyed by block number.
    block_hashes: SharedBlockHashRegistry,
    /// Optional P2P handle for broadcasting unsafe blocks to a test transport.
    supervised_p2p: Option<SupervisedP2P>,
    /// Optional pinned L1 origin. When set, epoch selection is bypassed and
    /// this block is used as the epoch for every subsequent L2 block built.
    l1_origin_pin: Option<BlockInfo>,
    /// Mutable L2 chain provider (for inserting new blocks/configs after each build).
    l2_provider: ActionL2ChainProvider,
    /// Optional conductor. When set, each build checks leadership via `leader()` and
    /// commits the sealed payload via `commit_unsafe_payload()` before inserting.
    conductor: Option<Arc<dyn Conductor>>,
    /// Optional signing key for gossip. When set, [`broadcast_unsafe_block`] computes the
    /// real [`PayloadHash`] and signs with the production formula instead of using a zero
    /// signature.
    ///
    /// [`broadcast_unsafe_block`]: L2Sequencer::broadcast_unsafe_block
    unsafe_block_signer: Option<PrivateKeySigner>,
}

impl L2Sequencer {
    /// Create a new sequencer using production components.
    pub fn new(
        head: L2BlockInfo,
        origin_selector: L1OriginSelector<SharedL1Chain>,
        attributes_builder: StatefulAttributesBuilder<ActionL1ChainProvider, ActionL2ChainProvider>,
        engine_client: Arc<ActionEngineClient>,
        rollup_config: Arc<RollupConfig>,
        l2_provider: ActionL2ChainProvider,
    ) -> Self {
        let test_account = Arc::new(Mutex::new(TestAccount::new(TEST_ACCOUNT_KEY)));
        let block_hashes = engine_client.block_hash_registry();

        Self {
            origin_selector,
            attributes_builder,
            engine_client,
            head,
            rollup_config,
            test_account,
            block_hashes,
            supervised_p2p: None,
            l1_origin_pin: None,
            l2_provider,
            conductor: None,
            unsafe_block_signer: None,
        }
    }

    /// Return the current unsafe L2 head.
    pub const fn head(&self) -> L2BlockInfo {
        self.head
    }

    /// Return a shared handle to the sequencer's test account.
    ///
    /// External test code can use this to build signed transactions with
    /// correct nonce tracking, independent of the sequencer.
    pub fn test_account(&self) -> Arc<Mutex<TestAccount>> {
        Arc::clone(&self.test_account)
    }

    /// Return the sequencer's shared block-hash registry.
    pub fn block_hash_registry(&self) -> SharedBlockHashRegistry {
        self.block_hashes.clone()
    }

    /// Return a clone of the sequencer's engine client.
    ///
    /// The derivation node can use this to share `executed_headers` with the
    /// sequencer so blocks pre-built by the sequencer are recognised as
    /// already-executed and not re-built from scratch during derivation.
    pub fn engine_client(&self) -> Arc<ActionEngineClient> {
        Arc::clone(&self.engine_client)
    }

    /// Read a storage value from the latest committed state via the engine client.
    ///
    /// Accepts the slot as a `U256` for convenience.
    /// Returns `U256::ZERO` if the account or slot does not exist.
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
    ///
    /// While pinned, every call to [`build_next_block_with_transactions`] uses `origin`
    /// as the epoch regardless of timestamps. The sequencer number increments within
    /// the same epoch until the pin is cleared.
    ///
    /// [`build_next_block_with_transactions`]: L2Sequencer::build_next_block_with_transactions
    pub const fn pin_l1_origin(&mut self, origin: BlockInfo) {
        self.l1_origin_pin = Some(origin);
    }

    /// Clear the pinned L1 origin, restoring automatic epoch selection.
    pub const fn clear_l1_origin_pin(&mut self) {
        self.l1_origin_pin = None;
    }

    /// Wire a [`SupervisedP2P`] handle to this sequencer.
    ///
    /// Once set, calling [`broadcast_unsafe_block`] delivers blocks to the
    /// matching [`TestGossipTransport`] receiver. Use
    /// [`ActionTestHarness::create_supervised_p2p`] to construct the pair and
    /// wire it in a single step.
    ///
    /// [`broadcast_unsafe_block`]: L2Sequencer::broadcast_unsafe_block
    /// [`ActionTestHarness::create_supervised_p2p`]: crate::ActionTestHarness::create_supervised_p2p
    pub fn set_supervised_p2p(&mut self, p2p: SupervisedP2P) {
        self.supervised_p2p = Some(p2p);
    }

    /// Attach an unsafe block signing key to this sequencer.
    ///
    /// Once set, [`broadcast_unsafe_block`] computes the real [`PayloadHash`]
    /// and signs it with the production formula:
    /// `keccak256(domain || chain_id_padded || keccak256(SSZ(payload)))`.
    ///
    /// Wire the corresponding address to the receiving [`TestGossipTransport`]
    /// via [`GossipTransport::set_block_signer`] and [`TestGossipTransport::set_chain_id`]
    /// to activate end-to-end signature validation.
    ///
    /// [`broadcast_unsafe_block`]: L2Sequencer::broadcast_unsafe_block
    /// [`GossipTransport::set_block_signer`]: base_consensus_node::GossipTransport::set_block_signer
    /// [`TestGossipTransport::set_chain_id`]: crate::TestGossipTransport::set_chain_id
    pub fn set_unsafe_block_signer(&mut self, key: PrivateKeySigner) {
        self.unsafe_block_signer = Some(key);
    }

    /// Return the address corresponding to the configured unsafe block signing key, if any.
    pub fn unsafe_block_signer_address(&self) -> Option<Address> {
        self.unsafe_block_signer.as_ref().map(|s| s.address())
    }

    /// Attach a conductor to this sequencer.
    ///
    /// Once set, every call to [`build_next_block_with_transactions`] first
    /// checks leadership via [`Conductor::leader`] and, after sealing,
    /// commits the payload via [`Conductor::commit_unsafe_payload`]. Build
    /// attempts return [`L2SequencerError::NotLeader`] when leadership is
    /// absent.
    ///
    /// Use [`TestConductorHandle::conductor`] to create a [`TestConductor`]
    /// and pass it here.
    ///
    /// [`build_next_block_with_transactions`]: L2Sequencer::build_next_block_with_transactions
    /// [`TestConductorHandle::conductor`]: crate::TestConductorHandle::conductor
    /// [`TestConductor`]: crate::TestConductor
    pub fn set_conductor(&mut self, conductor: Arc<dyn Conductor>) {
        self.conductor = Some(conductor);
    }

    /// Broadcast `block` as a [`NetworkPayloadEnvelope`] to the wired
    /// [`SupervisedP2P`] handle.
    ///
    /// A no-op when no handle has been set via [`set_supervised_p2p`].
    ///
    /// When an unsafe block signing key is configured via
    /// [`set_unsafe_block_signer`], the envelope is signed with the production
    /// formula (`keccak256(domain || chain_id_padded || keccak256(SSZ(payload)))`).
    /// Otherwise the envelope carries a zero signature, which passes through
    /// transports that have no expected signer configured.
    ///
    /// [`set_supervised_p2p`]: L2Sequencer::set_supervised_p2p
    /// [`set_unsafe_block_signer`]: L2Sequencer::set_unsafe_block_signer
    pub fn broadcast_unsafe_block(&self, block: &BaseBlock) {
        let Some(p2p) = &self.supervised_p2p else { return };
        let block_hash = block.header.hash_slow();
        let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(block_hash, block);
        let parent_beacon_block_root = block.header.parent_beacon_block_root;

        let (signature, payload_hash) = self.unsafe_block_signer.as_ref().map_or_else(
            || (Signature::new(U256::ZERO, U256::ZERO, false), PayloadHash(B256::ZERO)),
            |signer| {
                let envelope = BaseExecutionPayloadEnvelope {
                    execution_payload: execution_payload.clone(),
                    parent_beacon_block_root,
                };
                let ph = envelope.payload_hash();
                let msg = ph.signature_message(self.rollup_config.l2_chain_id.id());
                let sig = signer.sign_hash_sync(&msg).expect("unsafe block signing must not fail");
                (sig, ph)
            },
        );

        p2p.send(NetworkPayloadEnvelope {
            payload: execution_payload,
            signature,
            payload_hash,
            parent_beacon_block_root,
        });
    }

    /// Build the next L2 block containing no user transactions.
    ///
    /// Useful for simulating forced-empty blocks at the sequencer drift boundary.
    ///
    /// # Panics
    ///
    /// Panics if the block cannot be built (e.g. missing L1 block data).
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

    /// Build the next L2 block and advance the internal head.
    ///
    /// Returns a fully-formed [`BaseBlock`] containing the L1-info deposit and
    /// any provided user transactions, built by the production engine.
    ///
    /// # Panics
    ///
    /// Panics if the block cannot be built (e.g. missing L1 block data or engine
    /// execution failure). Use [`try_build_next_block_with_transactions`] if you need
    /// to inspect the error.
    ///
    /// [`try_build_next_block_with_transactions`]: L2Sequencer::try_build_next_block_with_transactions
    pub async fn build_next_block_with_transactions(
        &mut self,
        transactions: Vec<BaseTxEnvelope>,
    ) -> BaseBlock {
        self.try_build_next_block_with_transactions(transactions)
            .await
            .unwrap_or_else(|e| panic!("L2Sequencer::build_next_block failed: {e}"))
    }

    /// Build the next L2 block, returning an error instead of panicking.
    ///
    /// Prefer [`build_next_block_with_transactions`] in test code; this method
    /// exists for callers that need to inspect the failure reason.
    ///
    /// [`build_next_block_with_transactions`]: L2Sequencer::build_next_block_with_transactions
    pub async fn try_build_next_block_with_transactions(
        &mut self,
        user_txs: Vec<BaseTxEnvelope>,
    ) -> Result<BaseBlock, L2SequencerError> {
        // 0. Conductor leadership check: refuse to build if this node is not the leader.
        if let Some(conductor) = &self.conductor {
            let is_leader = conductor.leader().await?;
            if !is_leader {
                return Err(L2SequencerError::NotLeader);
            }
        }

        // 1. Origin selection: use pinned origin if set, otherwise production L1OriginSelector.
        let l1_origin = if let Some(pin) = self.l1_origin_pin {
            pin
        } else {
            self.origin_selector
                .next_l1_origin(self.head, false)
                .await
                .map_err(|e| L2SequencerError::OriginSelection(e.to_string()))?
        };

        // 2. Attribute construction via production StatefulAttributesBuilder.
        let epoch = BlockNumHash { number: l1_origin.number, hash: l1_origin.hash };
        let mut attrs = self
            .attributes_builder
            .prepare_payload_attributes(self.head, epoch)
            .await
            .map_err(|e| L2SequencerError::Attributes(format!("{e}")))?;

        // 3. Inject user transactions (encoded as Bytes) after the deposit txs.
        let encoded_user_txs: Vec<Bytes> = user_txs
            .iter()
            .map(|tx| {
                let mut buf = Vec::new();
                tx.encode_2718(&mut buf);
                Bytes::from(buf)
            })
            .collect();
        if let Some(txs) = &mut attrs.transactions {
            txs.extend(encoded_user_txs);
        }
        attrs.no_tx_pool = Some(true);

        // 4. Build via production engine client.
        let attrs_with_parent = AttributesWithParent::new(attrs, self.head, None, false);
        let payload_id = self
            .engine_client
            .start_build_block(attrs_with_parent.clone())
            .await
            .map_err(|e| L2SequencerError::Engine(format!("start_build: {e}")))?;

        let envelope = self
            .engine_client
            .get_sealed_payload(payload_id, attrs_with_parent)
            .await
            .map_err(|e| L2SequencerError::Engine(format!("get_sealed: {e}")))?;

        // 5. Conductor commit: register the sealed payload before inserting.
        // Map ConductorError::NotLeader to L2SequencerError::NotLeader so that
        // callers get the same variant regardless of whether leadership was lost
        // before the build started (pre-check) or between the check and commit
        // (TOCTOU).
        if let Some(conductor) = &self.conductor {
            conductor.commit_unsafe_payload(&envelope).await.map_err(|e| match e {
                ConductorError::NotLeader => L2SequencerError::NotLeader,
                other => L2SequencerError::Conductor(other),
            })?;
        }

        // 6. Insert the block into the engine (updates canonical head).
        self.engine_client
            .insert_unsafe_payload(envelope.clone())
            .await
            .map_err(|e| L2SequencerError::Engine(format!("insert: {e}")))?;

        // 7. Convert BaseExecutionPayload to BaseBlock.
        // Use try_into_block_with_sidecar so PBBR and requests_hash are restored on the
        // returned header. try_into_block() omits these fields, making hash_slow() return a
        // different value than the sealed block hash. BatchEncoder::add_block tracks self.tip
        // via block.header.hash_slow(), so missing sidecar fields cause block N+1's parent_hash
        // (the canonical hash of block N) to not match self.tip, triggering
        // ReorgError::ParentMismatch and resetting the encoder.
        //
        // V4 payloads (Isthmus+) require PraguePayloadFields with EMPTY_REQUESTS_HASH so that
        // the reconstructed header's requests_hash = Some(EMPTY_REQUESTS_HASH) matches reth's
        // canonical header.
        let block_hash = envelope.execution_payload.as_v1().block_hash;
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
        let block: BaseBlock = envelope
            .execution_payload
            .try_into_block_with_sidecar(&sidecar)
            .map_err(|e| L2SequencerError::PayloadConversion(format!("{e}")))?;

        // 8. Compute seq_num and update head.
        let seq_num =
            if l1_origin.number == self.head.l1_origin.number { self.head.seq_num + 1 } else { 0 };
        let block_number = block.header.number;
        let block_timestamp = block.header.timestamp;

        self.head = L2BlockInfo {
            block_info: BlockInfo {
                number: block_number,
                timestamp: block_timestamp,
                parent_hash: self.head.block_info.hash,
                hash: block_hash,
            },
            l1_origin: BlockNumHash { number: l1_origin.number, hash: l1_origin.hash },
            seq_num,
        };

        // 9. Update L2 provider state for next iteration.
        self.l2_provider.insert_block(self.head);
        // The system config is updated via the attributes builder's internal
        // L2 chain provider when the epoch changes. For the sequencer's
        // L2 provider copy, inherit the genesis config — the attributes
        // builder reads the correct config from its own provider clone.

        Ok(block)
    }
}
