//! Real MDBX/MPT semantic fixtures. Set BASE_DEFERRED_TEST_WIRING=legacy to select stock
//! commitment wiring for the must-hold baseline failures; the default exercises the candidate.
//! No provider root scalar is scripted. Each block transfers value using real EVM execution.

use std::{collections::VecDeque, io::Write, sync::{Arc, Mutex, mpsc}, time::{Duration, Instant}};
use alloy_consensus::{BlockBody, Header, SignableTransaction, Transaction, TxEip1559, TxReceipt, EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use base_common_consensus::{BaseBlock, BasePrimitives, BaseTxEnvelope, TxDeposit};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_consensus::{BaseBeaconConsensus, calculate_receipt_root_no_memo};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{BasePayloadTypes, BaseBuiltPayload, BasePayloadBuilderAttributes, ParkablePayloadTransactions, PostStateRootCohort, PostStateRootService, ResourceMeteringConfig, ResourceMeteringDimension, ResourceMeteringSchedule, builder::{BasePayloadBuilderCtx, Builder}, config::BaseBuilderConfig, payload::EthPayloadBuilderAttributes};
use base_execution_txpool::BasePooledTransaction;
use reth_basic_payload_builder::{BuildOutcomeKind, PayloadConfig};
use reth_payload_util::{NoopPayloadTransactions, PayloadTransactions};
use reth_transaction_pool::{TransactionOrigin, ValidPoolTransaction, identifier::TransactionId};
use alloy_eips::eip2718::Encodable2718;
use alloy_rpc_types_engine::PayloadId;
use base_node_core::{BaseNode, DeferredEngineValidator, DeferredStateRootState, DeferredStateRootStrategy, engine::BaseEngineValidator};
use reth_chainspec::EthChainSpec;
use reth_chain_state::CanonicalInMemoryState;
use reth_db_common::init::init_genesis;
use reth_engine_tree::{chain::FromOrchestrator, engine::{EngineApiKind, EngineApiRequest, FromEngine}, persistence::PersistenceHandle, tree::{BasicEngineValidator, EngineApiTreeHandler, EngineValidator, TreeConfig}};
use reth_evm::{ConfigureEvm, database::StateProviderDatabase, execute::Executor};
use reth_node_api::{BeaconEngineMessage, NoopInvalidBlockHook, NodeTypesWithDBAdapter};
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{BuiltPayload, BuiltPayloadExecutedBlock, PayloadTypes};
use reth_primitives_traits::{Block, RecoveredBlock, SealedHeader, SignedTransaction, WithEncoded};
use reth_provider::{BlockExecutionOutput, BlockWriter, ExecutionOutcome, HashedPostStateProvider, LatestStateProviderRef, ProviderFactory, StateProvider, StateProviderFactory, StateRootProvider, providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types};
use reth_storage_overlay::OverlayManager;
use reth_tasks::Runtime;
use reth_trie_common::{HashedPostState, KeccakKeyHasher, updates::TrieUpdates};
use tokio::sync::{mpsc::unbounded_channel, oneshot};

mod prewarm_support;
pub use prewarm_support::{PopulatedPrewarmSource, SnapshotEvents, SnapshotFields};

/// Concrete disposable Base database used by both independent execution paths.
pub type TestTypes = NodeTypesWithDBAdapter<BaseNode, Arc<reth_db::test_utils::TempDatabase<reth_db::DatabaseEnv>>>;
/// A fixture with independently computed roots and finalized EVM output.
#[derive(Debug)]
pub struct Fixture {
    /// Disposable MDBX, static-file and RocksDB provider factory.
    pub factory: ProviderFactory<TestTypes>,
    /// Matching deterministic chain configuration.
    pub chain: Arc<BaseChainSpec>,
    /// Sealed executed block with the stock same-block root.
    pub block: RecoveredBlock<BaseBlock>,
    /// Finalized EVM state and receipts.
    pub output: BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>,
    /// Independently hashed finalized state.
    pub hashed: HashedPostState,
    /// Actual MPT node updates, not fixture scalars.
    pub updates: TrieUpdates,
    /// Exact parent identity and legacy commitment.
    pub parent: SealedHeader<Header>,
    /// Independently computed actual current post-state root.
    pub root: B256,
}

impl Fixture {
    /// Initializes real genesis state and executes a deterministic transfer against it.
    pub fn new() -> Self {
        let tx = TxEip1559 {
            chain_id: 8453, nonce: 0, gas_limit: 21_000, max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1, to: TxKind::Call(Address::repeat_byte(0x42)),
            value: U256::from(7), ..Default::default()
        };
        let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
        let tx = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered = tx.clone().try_into_recovered().expect("deterministic signature recovers");
        let mut genesis: serde_json::Value = serde_json::from_str(include_str!("assets/genesis.json")).unwrap();
        genesis["alloc"][format!("{:#x}", recovered.signer())] = serde_json::json!({"balance": "0xd3c21bcecceda1000000"});
        let chain = Arc::new(BaseChainSpecBuilder::default().chain(BaseChainSpec::mainnet().chain)
            .genesis(serde_json::from_value(genesis).unwrap()).bedrock_activated().build());
        let factory = create_test_provider_factory_with_node_types::<BaseNode>(chain.clone());
        init_genesis(&factory).expect("real genesis initializes");
        let parent = SealedHeader::seal_slow(chain.genesis_header().clone());
        let block = BaseBlock::new(Header {
            parent_hash: parent.hash(), number: 1, timestamp: 2, gas_limit: 30_000_000,
            gas_used: 21_000, base_fee_per_gas: chain.next_block_base_fee(parent.header(), 2),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            transactions_root: alloy_consensus::proofs::calculate_transaction_root(&[tx.clone()]),
            ..Default::default()
        }, BlockBody { transactions: vec![tx], ..Default::default() }).try_into_recovered().unwrap();
        Self::execute(factory, chain, parent, block)
    }

    /// Adds a real genesis contract without changing the immutable stock-golden fixture.
    pub fn with_contract(code: &str) -> (Self, Address) {
        let original = Self::new(); let contract = Address::repeat_byte(0x66);
        let mut genesis = serde_json::to_value(original.chain.genesis()).unwrap();
        genesis["alloc"][format!("{contract:#x}")] = serde_json::json!({"balance":"0x0", "code":code});
        let chain = Arc::new(BaseChainSpecBuilder::default().chain(original.chain.chain).genesis(serde_json::from_value(genesis).unwrap()).bedrock_activated().build());
        let factory = create_test_provider_factory_with_node_types::<BaseNode>(chain.clone()); init_genesis(&factory).unwrap();
        let parent = SealedHeader::seal_slow(chain.genesis_header().clone());
        let mut raw = original.block.into_block(); raw.header.parent_hash = parent.hash();
        (Self::execute(factory, chain, parent, raw.try_into_recovered().unwrap()), contract)
    }

    /// Calculates actual MPT nodes from finalized execution, independently of header.state_root.
    pub fn execute(factory: ProviderFactory<TestTypes>, chain: Arc<BaseChainSpec>, parent: SealedHeader<Header>, mut block: RecoveredBlock<BaseBlock>) -> Self {
        let provider = factory.provider().unwrap();
        let state = LatestStateProviderRef::new(&provider);
        let output: BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt> = BaseEvmConfig::<BaseChainSpec, BasePrimitives>::new(chain.clone()).batch_executor(StateProviderDatabase::new((&state).into_evm_state_provider()))
            .execute(&block).expect("deterministic transfer executes");
        let hashed = state.hashed_post_state(&output.state).unwrap();
        let (root, updates) = state.state_root_with_updates(hashed.clone()).unwrap();
        block.set_state_root(root);
        let mut unsealed = block.into_block();
        unsealed.header.receipts_root = calculate_receipt_root_no_memo(&output.receipts, &chain, unsealed.header.timestamp);
        unsealed.header.logs_bloom = output.receipts.iter().fold(Default::default(), |bloom, r| bloom | r.bloom());
        let block = unsealed.try_into_recovered().unwrap();
        drop(provider);
        Self { factory, chain, block, output, hashed, updates, parent, root }
    }

    /// Persists actual state, then independently executes the next signed transaction at the next height.
    pub fn next(self) -> Self {
        let writer = self.factory.provider_rw().unwrap();
        let outcome = ExecutionOutcome::new(self.output.state.clone(), vec![self.output.receipts.clone()], self.block.number, vec![self.output.requests.clone()]);
        writer.append_blocks_with_state(vec![self.block.clone()], &outcome, self.hashed.into_sorted()).unwrap();
        writer.commit().unwrap();
        let parent = SealedHeader::seal_slow(self.block.header().clone());
        let mut next = self.block.into_block();
        next.header.parent_hash = parent.hash(); next.header.number += 1; next.header.timestamp += 2;
        next.header.base_fee_per_gas = self.chain.next_block_base_fee(parent.header(), next.header.timestamp);
        if let BaseTxEnvelope::Eip1559(tx) = &next.body.transactions[0] {
            let mut tx = tx.tx().clone(); tx.nonce += 1;
            let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
            next.body.transactions = vec![BaseTxEnvelope::Eip1559(tx.into_signed(signature))];
        }
        next.header.transactions_root = alloy_consensus::proofs::calculate_transaction_root(&next.body.transactions);
        Self::execute(self.factory, self.chain, parent, next.try_into_recovered().unwrap())
    }

    /// Builds through the production native builder using an owned real parent snapshot.
    pub fn native_payload(&self, service: Arc<PostStateRootService>) -> BaseBuiltPayload {
        self.native_with_pool(service, NoopPayloadTransactions::<Arc<ValidPoolTransaction<BasePooledTransaction>>>::default(), true, ResourceMeteringConfig::default())
    }

    /// Runs real candidates through production commit callbacks and an owned parent snapshot.
    pub fn native_with_pool<Txs>(&self, service: Arc<PostStateRootService>, candidates: Txs, no_tx_pool: bool, resource_metering: ResourceMeteringConfig) -> BaseBuiltPayload
    where Txs: ParkablePayloadTransactions<Pooled = BasePooledTransaction> + Send + Sync,
    {
        let provider = BlockchainProvider::new(self.factory.clone()).unwrap();
        let proof = provider.state_by_block_hash(self.parent.hash()).unwrap();
        let snapshot = provider.state_by_block_hash(self.parent.hash()).unwrap();
        let id = PayloadId::new([0; 8]);
        let attributes = BasePayloadBuilderAttributes {
            payload_attributes: EthPayloadBuilderAttributes { id, parent: self.parent.hash(), timestamp: self.block.timestamp, ..Default::default() },
            transactions: self.block.body().transactions.iter().map(|tx| WithEncoded::new(tx.encoded_2718().into(), tx.clone())).collect(),
            no_tx_pool, gas_limit: Some(self.block.gas_limit), ..Default::default()
        };
        let ctx = BasePayloadBuilderCtx {
            evm_config: BaseEvmConfig::<BaseChainSpec, BasePrimitives>::new(self.chain.clone()),
            builder_config: BaseBuilderConfig { deferred_state_root: Some(service), resource_metering, ..Default::default() },
            chain_spec: self.chain.clone(), config: PayloadConfig::new(Arc::new(self.parent.clone()), attributes, id),
            cancel: Default::default(), best_payload: None,
        };
        let outcome = Builder::new(|_, _| candidates)
            .with_root_snapshot(snapshot).build(StateProviderDatabase::new((&proof).into_evm_state_provider()), proof.as_ref(), None, ctx).expect("real native fixture must build");
        match outcome { BuildOutcomeKind::Freeze(payload) | BuildOutcomeKind::Better { payload } => payload, other => panic!("real native fixture must complete a payload: {other:?}") }
    }

    /// Submits a resealed header through the real BasicEngineValidator and Engine API tree.
    pub fn submit(&self, commitment: B256, deferred: bool) -> alloy_rpc_types_engine::PayloadStatus {
        let activation = if deferred && std::env::var("BASE_DEFERRED_TEST_WIRING").as_deref() != Ok("legacy") { Some(2) } else { None };
        self.submit_with_service(commitment, Arc::new(PostStateRootService { activation, ..Default::default() }), true)
    }

    /// Validates independently against shared builder provenance without draining between blocks.
    pub fn submit_with_service(&self, commitment: B256, service: Arc<PostStateRootService>, drain: bool) -> alloy_rpc_types_engine::PayloadStatus {
        self.submit_options(commitment, service, drain, TreeConfig::default(), Box::new(NoopInvalidBlockHook::default()), true)
    }

    /// Selects stock/wrapped validation with identical hook/prewarming options on real storage.
    pub fn submit_options(&self, commitment: B256, service: Arc<PostStateRootService>, drain: bool, config: TreeConfig, hook: Box<dyn reth_node_api::InvalidBlockHook<BasePrimitives>>, wrapped: bool) -> alloy_rpc_types_engine::PayloadStatus {
        self.submit_pool_options(commitment, service, drain, config, hook, wrapped, None)
    }

    /// Waits for real populated pool snapshots when requested, before independent validation.
    pub fn submit_pool_options(&self, commitment: B256, service: Arc<PostStateRootService>, drain: bool, config: TreeConfig, hook: Box<dyn reth_node_api::InvalidBlockHook<BasePrimitives>>, wrapped: bool, pool: Option<(PopulatedPrewarmSource, SnapshotEvents)>) -> alloy_rpc_types_engine::PayloadStatus {
        let runtime = Runtime::test();
        let provider = BlockchainProvider::new(self.factory.clone()).unwrap();
        let overlay = OverlayManager::new(runtime.state_trie_overlay_worker_pool());
        let evm = BaseEvmConfig::new(self.chain.clone());
        let consensus = Arc::new(BaseBeaconConsensus::new(self.chain.clone()));
        let activation = service.activation;
        let state = Arc::new(DeferredStateRootState { service, ..Default::default() });
        let inner = BasicEngineValidator::new(provider.clone(), consensus.clone(), evm.clone(),
            BaseEngineValidator::<BaseTxEnvelope, _>::new::<KeccakKeyHasher>(self.chain.clone()),
            config.clone(), hook, overlay.clone(), runtime.clone())
            .with_state_root_strategy(Arc::new(DeferredStateRootStrategy { provider: provider.clone(), overlay: overlay.clone(), state: state.clone(), stock: Default::default() }));
        let snapshot_events = pool.as_ref().map(|(_, events)| events.clone());
        let inner = if let Some((source, _)) = pool { inner.with_txpool_prewarming(source) } else { inner };
        let (persistence, _rx) = mpsc::channel(); let (payloads, _prx) = unbounded_channel();
        let (sender, _events) = if wrapped {
            EngineApiTreeHandler::spawn_new(provider, consensus, DeferredEngineValidator { inner, state: state.clone() },
                PersistenceHandle::new(persistence), PayloadBuilderHandle::<BasePayloadTypes>::new(payloads),
                CanonicalInMemoryState::with_head(self.parent.clone(), None, None), overlay, config,
                EngineApiKind::OpStack, evm, runtime.clone())
        } else {
            EngineApiTreeHandler::spawn_new(provider, consensus, inner,
                PersistenceHandle::new(persistence), PayloadBuilderHandle::<BasePayloadTypes>::new(payloads),
                CanonicalInMemoryState::with_head(self.parent.clone(), None, None), overlay, config,
                EngineApiKind::OpStack, evm, runtime.clone())
        };
        if let Some(events) = snapshot_events {
            let (tx, rx) = oneshot::channel();
            let forkchoice_state = alloy_rpc_types_engine::ForkchoiceState { head_block_hash: self.parent.hash(), safe_block_hash: self.parent.hash(), finalized_block_hash: self.parent.hash() };
            sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::ForkchoiceUpdated { cause: tracing::Span::none(), state: forkchoice_state, payload_attrs: None, tx }))).unwrap();
            let fcu = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), async { rx.await.unwrap().unwrap().await.unwrap() }).await.unwrap() });
            assert_eq!(fcu.payload_status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "parent FCU must start warming exactly the valid genesis state");
            let deadline = Instant::now() + Duration::from_secs(10);
            while events.0.lock().unwrap().is_empty() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(1)); }
            assert_eq!(events.0.lock().unwrap().len(), 1, "real txpool must publish exactly one populated snapshot before validation");
        }
        let mut block = self.block.clone().into_block(); block.header.state_root = commitment;
        let payload = BasePayloadTypes::<BasePrimitives>::block_to_payload(block.seal_slow(), None);
        let (tx, rx) = oneshot::channel();
        sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::NewPayload { cause: tracing::Span::none(), payload, tx }))).unwrap();
        let status = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.expect("engine response bounded").unwrap().expect("no operational error in semantic fixture") });
        let (tx, rx) = oneshot::channel(); sender.send(FromEngine::Event(FromOrchestrator::Terminate { tx })).unwrap();
        runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.unwrap().unwrap(); });
        drop(sender);
        if let Some(manager) = state.service.manager.get() {
            if status.status == alloy_rpc_types_engine::PayloadStatusEnum::Valid {
                let mut sealed = self.block.clone().into_block(); sealed.header.state_root = commitment;
                let key = base_execution_payload_builder::PostStateRootKey { hash: sealed.hash_slow(), parent_hash: sealed.header.parent_hash, number: sealed.header.number };
                let data = manager.get(key, base_execution_payload_builder::PostStateRootSource::Validator).unwrap().expect("VALID must publish actual validator trie data");
                assert_eq!(data.root, self.root, "VALID must publish the independently computed current MPT root, not the echoed parent commitment");
                assert_eq!(*data.updates, self.updates, "VALID must publish exact real current trie updates before admission");
            }
            if drain { manager.drain().expect("owned root worker drains after engine termination"); }
        }
        if activation.is_none() && state.service.cohort == base_execution_payload_builder::PostStateRootCohort::Ordinary { assert!(state.service.manager.get().is_none(), "off path allocates exactly zero root managers"); }
        runtime.shutdown_timeout(Duration::from_secs(10)); status
    }
}

/// In-flight callback recording needs one ordered log across commit and invalidation methods.
/// A static mock cannot own and advance the same candidate queue while those callbacks mutate it.
#[derive(Debug)]
pub struct RealCandidates {
    /// Actual signed and recovered pool transactions.
    pub queue: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    /// Ordered observable decisions from the production commit callbacks.
    pub decisions: Arc<Mutex<Vec<&'static str>>>,
}
impl PayloadTransactions for RealCandidates {
    type Transaction = Arc<ValidPoolTransaction<BasePooledTransaction>>;
    fn next(&mut self, _: ()) -> Option<Self::Transaction> { self.queue.pop_front() }
    fn mark_invalid(&mut self, _: Address, _: u64) { self.decisions.lock().unwrap().push("invalid"); }
}
impl ParkablePayloadTransactions for RealCandidates {
    type Pooled = BasePooledTransaction;
    fn park_current(&mut self) { panic!("plain signed transactions must not be predicate-parked"); }
    fn mark_current_committed(&mut self) { self.decisions.lock().unwrap().push("commit"); }
    fn promote(&mut self, _: B256) -> bool { false }
    fn discard_parked(&mut self, _: B256) -> bool { false }
}

/// Actual detached root includes a deposit and CommitChanges::Yes but not speculative SSTORE No.
#[test]
fn native_finalized_root_preserves_commit_yes_no_and_forced_deposit() {
    let (mut fixture, contract) = Fixture::with_contract("0x600160005500");
    let deposit: BaseTxEnvelope = TxDeposit { from: Address::repeat_byte(0x99), to: Address::repeat_byte(0x88).into(), mint: 100, value: U256::from(7), gas_limit: 30_000, ..Default::default() }.into();
    let mut raw = fixture.block.clone().into_block(); raw.body.transactions = vec![deposit.clone()]; fixture.block = raw.try_into_recovered().unwrap();
    let signed = |nonce, to| {
        let tx = TxEip1559 { chain_id: 8453, nonce, gas_limit: 100_000, max_fee_per_gas: 2_000_000_000, max_priority_fee_per_gas: 1, to: TxKind::Call(to), value: U256::from(1), ..Default::default() };
        let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
        BaseTxEnvelope::Eip1559(tx.into_signed(signature))
    };
    let accepted = signed(0, Address::repeat_byte(0x42)); let rejected = signed(1, contract);
    let candidates = [accepted.clone(), rejected.clone()].into_iter().map(|tx| {
        let transaction = BasePooledTransaction::new(tx.clone().try_into_recovered().unwrap(), tx.encode_2718_len());
        Arc::new(ValidPoolTransaction { transaction_id: TransactionId::new(0.into(), transaction.nonce()), transaction, propagate: false, timestamp: Instant::now(), origin: TransactionOrigin::External, authority_ids: None })
    }).collect();
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let schedule = ResourceMeteringSchedule::new(vec![ResourceMeteringDimension { name: "gas".into(), block_limit: 1_000_000, transaction_limit: 30_000, base_gas_weight: 1, operations: vec![], dry_run: false }]).compile().unwrap();
    let service = Arc::new(PostStateRootService { activation: Some(2), ..Default::default() });
    let payload = fixture.native_with_pool(service.clone(), RealCandidates { queue: candidates, decisions: decisions.clone() }, false, ResourceMeteringConfig { enabled: true, schedule: Arc::new(schedule), ..Default::default() });
    assert_eq!(*decisions.lock().unwrap(), vec!["commit", "invalid"], "native pool must commit exactly the transfer and reject exactly the executed over-budget SSTORE");
    assert_eq!(payload.block().body().transactions, vec![deposit, accepted], "final native body must include forced deposit and committed transfer only");
    assert!(payload.executed_block().is_none(), "deferred pool result must never expose a trusted insertion bypass");
    let checked = Fixture::execute(fixture.factory.clone(), fixture.chain.clone(), fixture.parent.clone(), payload.block().clone().into_block().try_into_recovered().unwrap());
    let key = base_execution_payload_builder::PostStateRootKey { hash: payload.block().hash(), parent_hash: payload.block().parent_hash, number: payload.block().number };
    let manager = service.manager.get().unwrap(); manager.wait_builder(key).unwrap();
    let built = manager.get(key, base_execution_payload_builder::PostStateRootSource::Builder).unwrap().unwrap();
    assert_eq!(built.root, checked.root, "detached root must exactly match independent execution of committed transactions plus forced deposit");
    assert_eq!(*built.updates, checked.updates, "detached trie nodes must exactly match authoritative committed and system/deposit changes");
    assert_eq!(checked.submit_with_service(fixture.parent.state_root, service.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "mixed finalized-state root must pass independent Engine execution and builder equivalence checks");
    manager.drain().unwrap();
}

/// Reverted storage writes and legacy SELFDESTRUCT are represented by finalized state, not hooks.
#[test]
fn native_finalized_root_handles_revert_and_account_deletion() {
    for (code, succeeds) in [("0x600160005560006000fd", false), ("0x730000000000000000000000000000000000000077ff", true)] {
        let (mut fixture, contract) = Fixture::with_contract(code);
        let BaseTxEnvelope::Eip1559(tx) = &fixture.block.body().transactions[0] else { panic!("fixture must start with a signed EIP1559 transaction"); };
        let mut tx = tx.tx().clone(); tx.to = contract.into(); tx.gas_limit = 100_000; tx.value = U256::ZERO;
        let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
        let mut raw = fixture.block.clone().into_block(); raw.body.transactions = vec![BaseTxEnvelope::Eip1559(tx.into_signed(signature))];
        fixture.block = raw.try_into_recovered().unwrap();
        let service = Arc::new(PostStateRootService { activation: Some(2), ..Default::default() });
        let payload = fixture.native_payload(service.clone());
        let checked = Fixture::execute(fixture.factory.clone(), fixture.chain.clone(), fixture.parent.clone(), payload.block().clone().into_block().try_into_recovered().unwrap());
        assert_eq!(checked.output.receipts.len(), 1, "each edge-case block must contain exactly one receipt");
        assert_eq!(checked.output.receipts[0].status(), succeeds, "receipt status must distinguish reverting SSTORE from successful legacy account deletion");
        let key = base_execution_payload_builder::PostStateRootKey { hash: payload.block().hash(), parent_hash: payload.block().parent_hash, number: payload.block().number };
        let manager = service.manager.get().unwrap(); manager.wait_builder(key).unwrap();
        let built = manager.get(key, base_execution_payload_builder::PostStateRootSource::Builder).unwrap().unwrap();
        assert_eq!(built.root, checked.root, "detached root must exactly match finalized revert/delete state, including nonce and fee effects");
        assert_eq!(*built.updates, checked.updates, "detached trie nodes must exactly preserve reverted writes and account deletion");
        assert_eq!(checked.submit_with_service(fixture.parent.state_root, service.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "edge-case finalized state must pass independent Engine validation");
        manager.drain().unwrap();
    }
}

/// Three actual blocks deploy, delete and recreate the same CREATE2 account under delayed headers.
#[test]
fn native_finalized_roots_preserve_delete_and_recreate_across_blocks() {
    let init_code = alloy_primitives::bytes!("6016600c60003960166000f3730000000000000000000000000000000000000077ff");
    let factory_code = format!("0x602260126000396001602260006000f55000{}", alloy_primitives::hex::encode(&init_code));
    let (mut fixture, factory_address) = Fixture::with_contract(&factory_code);
    let child = factory_address.create2_from_code(U256::from(1).to_be_bytes::<32>(), &init_code);
    let service = Arc::new(PostStateRootService { activation: Some(2), ..Default::default() });
    let mut roots = Vec::new();
    for (index, target) in [factory_address, child, factory_address].into_iter().enumerate() {
        let BaseTxEnvelope::Eip1559(tx) = &fixture.block.body().transactions[0] else { panic!("CREATE2 fixture must use signed EIP1559 transactions"); };
        let mut tx = tx.tx().clone(); tx.nonce = index as u64; tx.to = target.into(); tx.gas_limit = 300_000; tx.value = U256::ZERO;
        let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
        let mut raw = fixture.block.clone().into_block(); raw.body.transactions = vec![BaseTxEnvelope::Eip1559(tx.into_signed(signature))]; fixture.block = raw.try_into_recovered().unwrap();
        let payload = fixture.native_payload(service.clone());
        let mut checked = Fixture::execute(fixture.factory.clone(), fixture.chain.clone(), fixture.parent.clone(), payload.block().clone().into_block().try_into_recovered().unwrap());
        assert_eq!(checked.output.receipts.len(), 1, "each deploy/delete/recreate block must contain exactly one receipt");
        assert!(checked.output.receipts[0].status(), "outer transaction must succeed at each lifecycle transition");
        let account = checked.hashed.accounts.get(&keccak256(child)).expect("each lifecycle transition must affect exactly the derived CREATE2 account");
        assert_eq!(account.is_some(), index != 1, "CREATE2 account must exist after deployment/recreation and be deleted only in the middle block");
        let key = base_execution_payload_builder::PostStateRootKey { hash: payload.block().hash(), parent_hash: payload.block().parent_hash, number: payload.block().number };
        let manager = service.manager.get().unwrap(); manager.wait_builder(key).unwrap();
        let built = manager.get(key, base_execution_payload_builder::PostStateRootSource::Builder).unwrap().unwrap();
        assert_eq!(built.root, checked.root, "every detached lifecycle root must equal independent actual MPT execution");
        assert_eq!(*built.updates, checked.updates, "every lifecycle transition must publish exact actual current trie updates");
        assert_eq!(checked.submit_with_service(payload.block().state_root, service.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "delete/recreate execution must independently certify against its branch-correct actual parent");
        roots.push(checked.root);
        checked.block.set_state_root(payload.block().state_root);
        // Persist the delayed header and actual state; the next build cannot trust that header.
        fixture = checked.next();
    }
    assert_eq!(roots.len(), 3, "the lifecycle fixture must produce exactly three post-state roots");
    assert!(roots[0] != roots[1] && roots[1] != roots[2] && roots[0] != roots[2], "deployment, deletion and recreation must have pairwise distinct actual post-state roots");
    service.manager.get().unwrap().drain().unwrap();
}

/// A cold/evicted accepted-parent lookup must recover actual persisted MPT state, not its header.
#[test]
fn persisted_parent_cache_miss_uses_actual_root_not_delayed_header() {
    let mut first = Fixture::new(); let actual_first_root = first.root;
    let initial = Arc::new(PostStateRootService { activation: Some(2), ..Default::default() });
    let first_payload = first.native_payload(initial.clone());
    assert_eq!(first.submit_with_service(first_payload.block().state_root, initial.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "first delayed block must independently validate before persistence");
    first.block.set_state_root(first_payload.block().state_root); initial.manager.get().unwrap().drain().unwrap();
    let next = first.next();
    assert_ne!(next.parent.state_root, actual_first_root, "persisted delayed header must demonstrably differ from its actual post-state root");
    let cold = Arc::new(PostStateRootService { activation: Some(2), limits: base_execution_payload_builder::PostStateRootLimits { ready: 2, ..Default::default() }, ..Default::default() });
    let payload = next.native_payload(cold.clone());
    assert_eq!(payload.block().state_root, actual_first_root, "cold native parent recovery must calculate actual DB MPT root rather than copying delayed parent.header.state_root");
    let parent_key = base_execution_payload_builder::PostStateRootKey { hash: next.parent.hash(), parent_hash: next.parent.parent_hash, number: next.parent.number };
    let manager = cold.manager.get().unwrap();
    assert!(manager.get(parent_key, base_execution_payload_builder::PostStateRootSource::Validator).unwrap().is_none(), "validator must genuinely miss the retained parent entry in this bounded cache fixture");
    assert_eq!(next.submit_with_service(payload.block().state_root, cold.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "independent validator must recover the same actual persisted parent root on its own cache miss");
    manager.drain().unwrap();
}

/// c1 records deterministic exact payload/header/receipt/state/trie/status bytes on real storage.
#[test]
fn c1_stock_golden() {
    let a = Fixture::new(); let b = Fixture::new();
    assert_ne!(a.root, a.parent.state_root, "transfer must change the independently computed MPT root");
    assert_eq!(a.block, b.block, "independent databases produce identical sealed block bytes");
    assert_eq!(a.output, b.output, "independent execution produces identical receipts and finalized state");
    assert_eq!(a.hashed, b.hashed, "independent execution produces identical hashed state");
    assert_eq!(a.updates, b.updates, "independent MPT computation produces identical trie updates");
    let status = a.submit(a.root, false);
    assert_eq!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "stock same-block commitment is VALID");
    assert_eq!(status.latest_valid_hash, Some(a.block.hash()), "stock VALID identifies the exact sealed hash");
    let hashed = a.hashed.clone().into_sorted(); let updates = a.updates.clone().into_sorted();
    let storages = hashed.storages.iter().collect::<std::collections::BTreeMap<_, _>>();
    let storage_nodes = updates.storage_tries_ref().iter().collect::<std::collections::BTreeMap<_, _>>();
    let golden = format!("{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}", a.block, a.output.receipts, hashed.accounts, storages, updates.account_nodes_ref(), storage_nodes, status, BasePayloadTypes::<BasePrimitives>::block_to_payload(a.block.clone().into_block().seal_slow(), None));
    eprintln!("BASELINE_GOLDEN_DIGEST={}", keccak256(golden.as_bytes()));
    assert_eq!(keccak256(golden.as_bytes()), alloy_primitives::b256!("12429246b6b6753619b2b0980eec53f7d37b90a21bd0b3a0bdd6ad31e4822e3b"), "off execution must retain the immutable pre-production baseline digest");
    if let Ok(path) = std::env::var("BASE_DEFERRED_GOLDEN_PATH") {
        std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap().write_all(golden.as_bytes()).unwrap();
    }
    if let Ok(path) = std::env::var("BASE_DEFERRED_COMPARE_GOLDEN") {
        assert_eq!(golden, std::fs::read_to_string(path).unwrap(), "candidate off must exactly match immutable baseline block, payload, receipts, hashed state, trie nodes and engine status");
    }
}

/// c2 a correct independently computed parent root must be accepted, not same-block compared.
#[test]
fn c2_independent_parent_commitment() {
    let f = Fixture::new();
    assert_ne!(f.root, f.parent.state_root, "parent and current roots must be distinct before the assertion");
    let status = f.submit(f.parent.state_root, true);
    assert_eq!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "deferred block must accept the independently computed real parent commitment");
}

/// c3 resealing a wrong deferred root must be INVALID with exactly the parent latestValidHash.
#[test]
fn c3_wrong_deferred_root_resealed() {
    let f = Fixture::new(); let status = f.submit(f.root, true);
    assert!(matches!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Invalid { .. }), "same-block root is the wrong deferred commitment and must be INVALID, got {status:?}");
    assert_eq!(status.latest_valid_hash, Some(f.parent.hash()), "wrong deferred commitment must identify exactly the valid parent");
}

/// Local trusted insertion must not bypass independent validator execution in R or D.
#[test]
fn deferred_and_reexecution_refuse_trusted_insertion() {
    for cohort in [PostStateRootCohort::SerialReexecution, PostStateRootCohort::Deferred] {
        let fixture = Fixture::new(); let runtime = Runtime::test();
        let provider = BlockchainProvider::new(fixture.factory.clone()).unwrap();
        let overlay = OverlayManager::new(runtime.state_trie_overlay_worker_pool());
        let service = Arc::new(PostStateRootService { cohort, activation: (cohort == PostStateRootCohort::Deferred).then_some(2), ..Default::default() });
        let inner = BasicEngineValidator::new(provider, Arc::new(BaseBeaconConsensus::new(fixture.chain.clone())), BaseEvmConfig::new(fixture.chain.clone()),
            BaseEngineValidator::<BaseTxEnvelope, _>::new::<KeccakKeyHasher>(fixture.chain.clone()), TreeConfig::default(), Box::new(NoopInvalidBlockHook::default()), overlay, runtime.clone());
        let validator = DeferredEngineValidator { inner, state: Arc::new(DeferredStateRootState { service: service.clone(), ..Default::default() }) };
        let block = BuiltPayloadExecutedBlock { recovered_block: Arc::new(fixture.block), execution_output: Arc::new(fixture.output), hashed_state: Arc::new(fixture.hashed), trie_updates: Arc::new(fixture.updates) };
        let result = <DeferredEngineValidator<_> as EngineValidator<BasePayloadTypes>>::on_inserted_executed_block(&validator, block);
        assert_eq!(result.unwrap_err().to_string(), "trusted insertion is unsupported for deferred state roots", "{cohort:?} must refuse trusted bypass before stock insertion");
        assert!(service.manager.get().is_none(), "rejected trusted bypass must allocate exactly zero provider workers");
        runtime.shutdown_timeout(Duration::from_secs(2));
    }
}

/// Deferred commitment equality must not waive ordinary gas or receipt consensus checks.
#[test]
fn deferred_non_root_consensus_failures_never_publish_ready_validator_data() {
    for bad_gas in [false, true] {
        let mut fixture = Fixture::new();
        let mut block = fixture.block.clone().into_block();
        if bad_gas { block.header.gas_used += 1; } else { block.header.receipts_root = B256::ZERO; }
        fixture.block = block.try_into_recovered().unwrap();
        let service = Arc::new(PostStateRootService { activation: Some(2), ..Default::default() });
        let status = fixture.submit_with_service(fixture.parent.state_root, service.clone(), true);
        assert!(matches!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Invalid { .. }), "deferred root must preserve exact ordinary gas/receipt rejection (gas={bad_gas}): {status:?}");
        assert_eq!(status.latest_valid_hash, Some(fixture.parent.hash()), "non-root consensus rejection must identify the exact valid parent");
        if let Some(manager) = service.manager.get() {
            assert_eq!(manager.ready.lock().unwrap().entries.len(), 0, "failed inner consensus checks must publish exactly zero certified validator roots");
        }
    }
}

/// c4 activation must commit to the independently computed parent, never the current state.
#[test]
fn c4_activation_parent_post_state() {
    let mut f = Fixture::new();
    let activation = if std::env::var("BASE_DEFERRED_TEST_WIRING").as_deref() == Ok("legacy") { None } else { Some(2) };
    let service = Arc::new(PostStateRootService { activation, ..Default::default() });
    let first_payload = f.native_payload(service.clone());
    let genesis_root = f.parent.state_root; let first_root = f.root;
    assert_ne!(genesis_root, first_root, "F-1 and F actual MPT roots must be distinct");
    let first_header_root = first_payload.block().state_root;
    if activation.is_some() { assert!(first_payload.executed_block().is_none(), "active native builds expose zero trusted executed objects"); }
    if activation.is_some() {
        assert_eq!(f.submit_with_service(first_header_root, service.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "native F must pass independent re-execution and compare builder provenance");
        f.block.set_state_root(first_header_root);
    }
    let next = f.next();
    let next_payload = next.native_payload(service.clone());
    assert_ne!(first_root, next.root, "F and F+1 actual MPT roots must be distinct");
    assert_ne!(genesis_root, next.root, "F-1 and F+1 actual MPT roots must be distinct");
    assert_eq!(first_header_root, genesis_root, "first active header F must commit R(F-1), not R(F)");
    assert_eq!(next_payload.block().state_root, first_root, "active header F+1 must commit independent R(F), not delayed header F");
    assert_eq!(next.submit_with_service(next_payload.block().state_root, service.clone(), false).status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "native F+1 with persisted delayed parent must validate using actual MPT state");
    service.manager.get().unwrap().drain().expect("all detached root work drains");
}

/// The serial cohorts hold commitment/root algorithm fixed and vary only trusted insertion.
#[test]
fn serial_controls_preserve_current_root_and_insertion_policy() {
    for cohort in [base_execution_payload_builder::PostStateRootCohort::SerialTrusted, base_execution_payload_builder::PostStateRootCohort::SerialReexecution] {
        let fixture = Fixture::new();
        let service = Arc::new(PostStateRootService { cohort, ..Default::default() });
        let payload = fixture.native_payload(service.clone());
        assert_eq!(payload.block().state_root, fixture.root, "S/R must commit the exact same-block independently computed root");
        assert_eq!(payload.executed_block().is_some(), cohort == base_execution_payload_builder::PostStateRootCohort::SerialTrusted, "only S preserves trusted insertion; R must force independent newPayload execution");
        let status = fixture.submit_with_service(payload.block().state_root, service.clone(), false);
        assert_eq!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "both serial controls must pass exact current-root validation");
        service.manager.get().unwrap().drain().unwrap();
    }
}

/// Successful S insertion must retire builder-only provenance without validator certification.
#[test]
fn serial_trusted_insertion_survives_ready_retention() {
    let mut fixture = Fixture::new();
    let runtime = Runtime::test();
    let service = Arc::new(PostStateRootService { cohort: PostStateRootCohort::SerialTrusted, limits: base_execution_payload_builder::PostStateRootLimits { ready: 2, ..Default::default() }, ..Default::default() });
    for height in 1..=5 {
        let payload = fixture.native_payload(service.clone());
        assert_eq!(payload.block().number, height, "native serial builds must advance through more than twice the retention cap");
        let provider = BlockchainProvider::new(fixture.factory.clone()).unwrap();
        let overlay = OverlayManager::new(runtime.state_trie_overlay_worker_pool());
        let inner = BasicEngineValidator::new(provider, Arc::new(BaseBeaconConsensus::new(fixture.chain.clone())), BaseEvmConfig::new(fixture.chain.clone()),
            BaseEngineValidator::<BaseTxEnvelope, _>::new::<KeccakKeyHasher>(fixture.chain.clone()), TreeConfig::default(), Box::new(NoopInvalidBlockHook::default()), overlay, runtime.clone());
        let validator = DeferredEngineValidator { inner, state: Arc::new(DeferredStateRootState { service: service.clone(), ..Default::default() }) };
        let inserted = <DeferredEngineValidator<_> as EngineValidator<BasePayloadTypes>>::on_inserted_executed_block(&validator, payload.executed_block().unwrap()).unwrap();
        assert_eq!(inserted.recovered_block.hash(), payload.block().hash(), "trusted insertion must preserve the exact native sealed block");
        let manager = service.manager.get().unwrap();
        manager.check_health().expect("successful serial trusted insertion must leave manager healthy");
        fixture = fixture.next();
    }
    let manager = service.manager.get().unwrap();
    let ready = manager.ready.lock().unwrap();
    assert_eq!(ready.entries.len(), 0, "accepted S builder provenance must retire instead of exhausting retention");
    assert_eq!(ready.order.len(), 0, "retirement must remove FIFO identities too");
    drop(ready);
    service.manager.get().unwrap().drain().unwrap();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

/// A configured but future fork must preserve the native builder's exact legacy payload.
#[test]
fn pre_fork_native_payload_parity() {
    let fixture = Fixture::new();
    let off = Arc::new(PostStateRootService::default());
    let future = Arc::new(PostStateRootService { activation: Some(3), ..Default::default() });
    let ordinary = fixture.native_payload(off.clone()); let pre_fork = fixture.native_payload(future.clone());
    assert_eq!(ordinary.block(), pre_fork.block(), "pre-F native header/body bytes must equal ordinary off payload exactly");
    assert_eq!(ordinary.executed_block(), pre_fork.executed_block(), "pre-F finalized receipts/state/trie updates must equal ordinary off exactly");
    assert!(off.manager.get().is_none() && future.manager.get().is_none(), "Never and pre-F build exactly zero root managers and enqueue zero jobs");
}

/// Blocking the current-root worker delays validation, not immutable native payload assembly.
#[test]
fn seal_does_not_wait_for_current_root() {
    let fixture = Fixture::new();
    let service = Arc::new(PostStateRootService { activation: Some(2), ..Default::default() });
    let manager = service.manager().unwrap();
    let (release, blocked) = mpsc::channel(); let (started, running) = mpsc::channel();
    let provider = fixture.factory.provider().unwrap();
    let parent_root = LatestStateProviderRef::new(&provider).state_root(HashedPostState::default()).unwrap();
    assert_eq!(parent_root, fixture.parent.state_root, "barrier fixture must first independently resolve and release the real parent root");
    drop(provider);
    manager.publish(base_execution_payload_builder::PostStateRootKey { hash: fixture.parent.hash(), parent_hash: fixture.parent.parent_hash, number: fixture.parent.number }, base_execution_payload_builder::PostStateRootSource::Validator, base_execution_payload_builder::PostStateRootData::new(parent_root, &HashedPostState::default(), TrieUpdates::default())).unwrap();
    let barrier = manager.enqueue(Box::new(move || {
        started.send(()).unwrap(); blocked.recv().unwrap();
        Ok(base_execution_payload_builder::PostStateRootData::new(B256::ZERO, &HashedPostState::default(), TrieUpdates::default()))
    })).unwrap();
    running.recv_timeout(Duration::from_secs(2)).unwrap();
    let payload = fixture.native_payload(service.clone());
    assert_eq!(payload.block().state_root, fixture.parent.state_root, "sealing with a blocked current-root worker must still use the independently computed parent root");
    assert_eq!(manager.builders.lock().unwrap().len(), 1, "exactly the sealed current-root job must remain uncompleted behind the barrier");
    assert_eq!(manager.ready.lock().unwrap().entries.len(), 1, "sealing alone must publish zero current roots beyond the already released parent");
    let commitment = payload.block().state_root;
    let (result, response) = mpsc::channel();
    let validation = std::thread::spawn(move || { result.send(fixture.submit_with_service(commitment, service, false)).unwrap(); });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while manager.admitted.load(std::sync::atomic::Ordering::Acquire) < 3 && std::time::Instant::now() < deadline { std::thread::sleep(Duration::from_millis(1)); }
    assert_eq!(manager.admitted.load(std::sync::atomic::Ordering::Acquire), 3, "barrier, detached builder and independent validator parent/root operation must all be admitted before checking acceptance");
    assert_eq!(response.try_recv(), Err(mpsc::TryRecvError::Empty), "validator must expose exactly zero Engine VALID responses while its actual root work is blocked");
    release.send(()).unwrap(); barrier.wait().unwrap();
    assert_eq!(response.recv_timeout(Duration::from_secs(10)).unwrap().status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "after release, independent validation may publish complete current trie data");
    validation.join().unwrap();
    manager.drain().unwrap();
}

/// Never/pre-F stock/wrapper validation receives a genuinely populated pool-prewarm snapshot.
#[test]
fn populated_txpool_prewarm_stock_wrapper_parity() {
    let events = SnapshotEvents::default(); tracing::subscriber::set_global_default(events.clone()).unwrap();
    for activation in [None, Some(3)] {
        for prewarming in [false, true] {
            for malformed in [false, true] {
                let mut observations = Vec::new();
                for wrapped in [false, true] {
                    events.0.lock().unwrap().clear();
                    let fixture = Fixture::new();
                    let recovered = fixture.block.body().transactions[0].clone().try_into_recovered().unwrap();
                    let source = PopulatedPrewarmSource(reth_engine_tree::tree::TxPoolPrewarmTransaction { hash: recovered.tx_hash(), sender: recovered.signer(), transaction: recovered });
                    let service = Arc::new(PostStateRootService { activation, ..Default::default() });
                    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0)); let counted = calls.clone();
                    let hook = Box::new(move |_: &SealedHeader<Header>, _: &RecoveredBlock<BaseBlock>, _: &BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>, _: Option<(&TrieUpdates, B256)>| { counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst); });
                    let status = fixture.submit_pool_options(if malformed { B256::repeat_byte(0x99) } else { fixture.root }, service.clone(), true, TreeConfig::default().without_prewarming(!prewarming), hook, wrapped, Some((source, events.clone())));
                    assert!(service.manager.get().is_none(), "populated Never/pre-F pool must allocate zero managers/jobs");
                    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), usize::from(malformed), "populated cache must preserve exact invalid hook behavior");
                    let snapshots = events.0.lock().unwrap().clone();
                    assert_eq!(snapshots, vec![(7, 2, 0)], "Base transfer prewarming including OP predeploy reads must publish exactly seven accounts, two slots and zero bytecodes before validation");
                    observations.push((status, snapshots));
                }
                assert_eq!(observations[0], observations[1], "stock/wrapper status/latestValidHash and populated snapshot counts must match exactly");
            }
        }
    }
}

/// Never/pre-F delegate hooks and prewarming options to stock, including genuine invalid roots.
#[test]
fn stock_wrapper_hook_and_prewarm_matrix() {
    for activation in [None, Some(3)] {
        for prewarming in [false, true] {
            for malformed in [false, true] {
                let stock = Fixture::new(); let candidate = Fixture::new();
                let stock_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let candidate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let commitment = if malformed { B256::repeat_byte(0x99) } else { stock.root };
                let config = TreeConfig::default().without_prewarming(!prewarming);
                let record = |calls: Arc<std::sync::atomic::AtomicUsize>| Box::new(move |_: &SealedHeader<Header>, _: &RecoveredBlock<BaseBlock>, _: &BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>, _: Option<(&TrieUpdates, B256)>| { calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }) as Box<dyn reth_node_api::InvalidBlockHook<BasePrimitives>>;
                let reference = stock.submit_options(commitment, Arc::new(PostStateRootService::default()), true, config.clone(), record(stock_calls.clone()), false);
                let service = Arc::new(PostStateRootService { activation, ..Default::default() });
                let observed = candidate.submit_options(commitment, service.clone(), true, config, record(candidate_calls.clone()), true);
                assert_eq!(observed, reference, "Never/pre-F wrapped engine status and exact latestValidHash must match stock for each hook/prewarm setting");
                assert_eq!(stock_calls.load(std::sync::atomic::Ordering::SeqCst), usize::from(malformed), "stock invalid-root hook must fire exactly once on malformed commitment and zero times on valid input");
                assert_eq!(candidate_calls.load(std::sync::atomic::Ordering::SeqCst), usize::from(malformed), "wrapper must preserve exact invalid hook invocation count");
                assert!(service.manager.get().is_none(), "Never/pre-F hook/prewarm matrix must construct exactly zero managers and root jobs");
            }
        }
    }
}

/// Deferred mismatches return the independently computed parent to a compatible recording hook.
#[test]
fn deferred_invalid_hook_observes_computed_parent_not_echo() {
    let fixture = Fixture::new();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new())); let sink = observed.clone();
    let hook = Box::new(move |_: &SealedHeader<Header>, _: &RecoveredBlock<BaseBlock>, _: &BlockExecutionOutput<<BasePrimitives as reth_primitives_traits::NodePrimitives>::Receipt>, trie: Option<(&TrieUpdates, B256)>| {
        sink.lock().unwrap().push(trie.map(|(updates, root)| (updates.clone(), root)));
    });
    let status = fixture.submit_options(fixture.root, Arc::new(PostStateRootService { activation: Some(2), ..Default::default() }), true, TreeConfig::default(), hook, true);
    assert!(status.status.is_invalid(), "wrong delayed commitment must be INVALID with compatible diagnostics enabled");
    assert_eq!(status.latest_valid_hash, Some(fixture.parent.hash()), "compatible diagnostic hook must not alter exact parent latestValidHash");
    assert_eq!(*observed.lock().unwrap(), vec![Some((TrieUpdates::default(), fixture.parent.state_root))], "exactly one mismatch-only callback must receive computed parent and rejection-only empty updates, never a same-block witness");
}

/// Competing in-memory branches keep actual roots while FCU crosses the activation boundary.
#[test]
fn memory_reorg_crosses_activation_and_preserves_same_height_roots() {
    let a = Fixture::new(); let genesis = a.parent.clone(); let a1 = a.block.clone(); let a1_root = a.root;
    let a2 = a.next();
    let mut b = Fixture::new(); let mut block = b.block.into_block(); block.header.timestamp = 4;
    let BaseTxEnvelope::Eip1559(tx) = &block.body.transactions[0] else { panic!("fixture must contain EIP1559 transfer"); };
    let mut tx = tx.tx().clone(); tx.value = U256::from(9);
    let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
    block.body.transactions = vec![BaseTxEnvelope::Eip1559(tx.into_signed(signature))];
    block.header.transactions_root = alloy_consensus::proofs::calculate_transaction_root(&block.body.transactions);
    b = Fixture::execute(b.factory, b.chain, b.parent, block.try_into_recovered().unwrap());
    let b1_root = b.root; b.block.set_state_root(genesis.state_root); let b1 = b.block.clone(); let b2 = b.next();
    assert_ne!(a1_root, b1_root, "same-height branch transfers must have independently distinct actual roots");
    assert_ne!(a2.root, b2.root, "same-height descendants must preserve their branch-specific actual roots");
    let db = Fixture::new(); let runtime = Runtime::test();
    let provider = BlockchainProvider::new(db.factory.clone()).unwrap();
    let overlay = OverlayManager::new(runtime.state_trie_overlay_worker_pool()); let config = TreeConfig::default().with_unwind_canonical_header(true);
    let canonical = CanonicalInMemoryState::with_head(genesis.clone(), None, None);
    let evm = BaseEvmConfig::new(db.chain.clone()); let consensus = Arc::new(BaseBeaconConsensus::new(db.chain.clone()));
    let state = Arc::new(DeferredStateRootState { service: Arc::new(PostStateRootService { activation: Some(3), ..Default::default() }), ..Default::default() });
    let inner = BasicEngineValidator::new(provider.clone(), consensus.clone(), evm.clone(), BaseEngineValidator::<BaseTxEnvelope, _>::new::<KeccakKeyHasher>(db.chain.clone()), config.clone(), Box::new(NoopInvalidBlockHook::default()), overlay.clone(), runtime.clone())
        .with_state_root_strategy(Arc::new(DeferredStateRootStrategy { provider: provider.clone(), overlay: overlay.clone(), state: state.clone(), stock: Default::default() }));
    let (persistence, _rx) = mpsc::channel(); let (payloads, _prx) = unbounded_channel();
    let (sender, _events) = EngineApiTreeHandler::spawn_new(provider, consensus, DeferredEngineValidator { inner, state: state.clone() }, PersistenceHandle::new(persistence), PayloadBuilderHandle::<BasePayloadTypes>::new(payloads), canonical.clone(), overlay, config, EngineApiKind::OpStack, evm, runtime.clone());
    let send = |block: &RecoveredBlock<BaseBlock>, root: B256| {
        let mut block = block.clone().into_block(); block.header.state_root = root; let block = block.seal_slow(); let hash = block.hash();
        let payload = BasePayloadTypes::<BasePrimitives>::block_to_payload(block, None); let (tx, rx) = oneshot::channel();
        sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::NewPayload { cause: tracing::Span::none(), payload, tx }))).unwrap();
        let status = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.unwrap().unwrap().unwrap() });
        assert_eq!(status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "each branch must independently validate against its exact real parent state");
        assert_eq!(status.latest_valid_hash, Some(hash), "VALID must identify the exact sealed branch hash"); hash
    };
    let fcu = |hash| {
        let (tx, rx) = oneshot::channel(); sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::ForkchoiceUpdated { cause: tracing::Span::none(), state: alloy_rpc_types_engine::ForkchoiceState { head_block_hash: hash, safe_block_hash: genesis.hash(), finalized_block_hash: genesis.hash() }, payload_attrs: None, tx }))).unwrap();
        let result = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), async { rx.await.unwrap().unwrap().await.unwrap() }).await.unwrap() });
        assert_eq!(result.payload_status.status, alloy_rpc_types_engine::PayloadStatusEnum::Valid, "memory forkchoice must remain VALID across the activation boundary");
        assert_eq!(result.payload_status.latest_valid_hash, Some(hash), "FCU must identify the exact selected branch head");
        assert_eq!(canonical.get_canonical_head().hash(), hash, "memory reorg must actually select the requested canonical branch, not merely return VALID");
    };
    let a_hash = send(&a1, a1_root); fcu(a_hash);
    let a2_hash = send(&a2.block, a1_root); fcu(a2_hash);
    let b_hash = send(&b1, genesis.state_root); let b2_hash = send(&b2.block, b1_root); fcu(b2_hash);
    for hash in [a_hash, b_hash, a2_hash, b2_hash] { fcu(hash); }
    let mut wrong = a2.block.clone().into_block(); wrong.header.state_root = a2.root;
    let wrong_hash = wrong.hash_slow();
    let mut descendant = wrong.clone(); descendant.header.parent_hash = wrong_hash; descendant.header.number += 1; descendant.header.timestamp += 2;
    descendant.header.base_fee_per_gas = db.chain.next_block_base_fee(&wrong.header, descendant.header.timestamp);
    let BaseTxEnvelope::Eip1559(tx) = &descendant.body.transactions[0] else { panic!("descendant fixture must be EIP1559"); };
    let mut tx = tx.tx().clone(); tx.nonce += 1;
    let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
    descendant.body.transactions = vec![BaseTxEnvelope::Eip1559(tx.into_signed(signature))];
    descendant.header.transactions_root = alloy_consensus::proofs::calculate_transaction_root(&descendant.body.transactions);
    for rejected in [wrong, descendant] {
        let payload = BasePayloadTypes::<BasePrimitives>::block_to_payload(rejected.seal_slow(), None); let (tx, rx) = oneshot::channel();
        sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::NewPayload { cause: tracing::Span::none(), payload, tx }))).unwrap();
        let status = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.unwrap().unwrap().unwrap() });
        assert!(status.status.is_invalid(), "wrong deferred commitment and its descendant must both be INVALID, not deferred SYNCING or trusted acceptance");
        assert_eq!(status.latest_valid_hash, Some(a_hash), "invalid commitment ancestry must retain exactly its last valid branch parent");
    }
    let (tx, rx) = oneshot::channel(); sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::ForkchoiceUpdated { cause: tracing::Span::none(), state: alloy_rpc_types_engine::ForkchoiceState { head_block_hash: wrong_hash, safe_block_hash: genesis.hash(), finalized_block_hash: genesis.hash() }, payload_attrs: None, tx }))).unwrap();
    let invalid_fcu = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), async { rx.await.unwrap().unwrap().await.unwrap() }).await.unwrap() });
    assert!(invalid_fcu.payload_status.status.is_invalid(), "FCU must not canonicalize an invalid deferred commitment");
    assert_eq!(invalid_fcu.payload_status.latest_valid_hash, Some(a_hash), "invalid FCU must preserve exact valid branch ancestry");
    assert_eq!(canonical.get_canonical_head().hash(), b2_hash, "invalid descendant/FCU attempts must leave the previously selected valid head unchanged");
    let manager = state.service.manager.get().unwrap();
    for (hash, parent_hash, number, expected) in [(a2_hash, a_hash, 2, a2.root), (b_hash, genesis.hash(), 1, b1_root), (b2_hash, b_hash, 2, b2.root)] {
        let key = base_execution_payload_builder::PostStateRootKey { hash, parent_hash, number };
        assert_eq!(manager.get(key, base_execution_payload_builder::PostStateRootSource::Validator).unwrap().unwrap().root, expected, "canonical switches must not replace another branch's independently computed actual root");
    }
    // This is a memory-reorg test, not a persistence-service double. Return to genesis so
    // termination has no canonical data to send to the deliberately unserved persistence handle.
    fcu(genesis.hash());
    let (tx, rx) = oneshot::channel(); sender.send(FromEngine::Event(FromOrchestrator::Terminate { tx })).unwrap();
    runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.unwrap().unwrap(); });
    drop(sender); manager.drain().unwrap(); runtime.shutdown_timeout(Duration::from_secs(10));
}
