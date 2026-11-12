//! Real MDBX/MPT semantic fixtures. Set BASE_DEFERRED_TEST_WIRING=deferred to select the
//! experimental wiring once installed; the default deliberately exercises the stock baseline.
//! No provider root scalar is scripted. Each block transfers value using real EVM execution.

use std::{io::Write, sync::{Arc, mpsc}, time::Duration};
use alloy_consensus::{BlockBody, BlockHeader, Header, SignableTransaction, TxEip1559, TxReceipt, EMPTY_OMMER_ROOT_HASH};
use alloy_primitives::{Address, B256, TxKind, U256, keccak256};
use base_common_consensus::{BaseBlock, BasePrimitives, BaseTxEnvelope};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_consensus::{BaseBeaconConsensus, calculate_receipt_root_no_memo};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::BasePayloadTypes;
use base_node_core::{BaseNode, DeferredEngineValidator, DeferredStateRootState, DeferredStateRootStrategy, engine::BaseEngineValidator};
use reth_chainspec::EthChainSpec;
use reth_chain_state::CanonicalInMemoryState;
use reth_db_common::init::init_genesis;
use reth_engine_tree::{chain::FromOrchestrator, engine::{EngineApiKind, EngineApiRequest, FromEngine}, persistence::PersistenceHandle, tree::{BasicEngineValidator, EngineApiTreeHandler, TreeConfig}};
use reth_evm::{ConfigureEvm, database::StateProviderDatabase, execute::Executor};
use reth_node_api::{BeaconEngineMessage, NoopInvalidBlockHook, NodeTypesWithDBAdapter};
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::PayloadTypes;
use reth_primitives_traits::{Block, RecoveredBlock, SealedHeader, SignedTransaction};
use reth_provider::{BlockExecutionOutput, BlockWriter, ExecutionOutcome, HashedPostStateProvider, LatestStateProviderRef, ProviderFactory, StateProvider, StateRootProvider, providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types};
use reth_storage_overlay::OverlayManager;
use reth_tasks::Runtime;
use reth_trie_common::{HashedPostState, KeccakKeyHasher, updates::TrieUpdates};
use tokio::sync::{mpsc::unbounded_channel, oneshot};

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

    /// Persists actual state, then independently executes a second transfer at a different height.
    pub fn next(self) -> Self {
        let writer = self.factory.provider_rw().unwrap();
        let outcome = ExecutionOutcome::new(self.output.state.clone(), vec![self.output.receipts.clone()], 1, vec![self.output.requests.clone()]);
        writer.append_blocks_with_state(vec![self.block.clone()], &outcome, self.hashed.into_sorted()).unwrap();
        writer.commit().unwrap();
        let parent = SealedHeader::seal_slow(self.block.header().clone());
        let mut next = self.block.into_block();
        next.header.parent_hash = parent.hash(); next.header.number = 2; next.header.timestamp = 4;
        next.header.base_fee_per_gas = self.chain.next_block_base_fee(parent.header(), 4);
        if let BaseTxEnvelope::Eip1559(tx) = &next.body.transactions[0] {
            let mut tx = tx.tx().clone(); tx.nonce = 1;
            let signature = reth_primitives_traits::crypto::secp256k1::sign_message(B256::repeat_byte(1), tx.signature_hash()).unwrap();
            next.body.transactions = vec![BaseTxEnvelope::Eip1559(tx.into_signed(signature))];
        }
        next.header.transactions_root = alloy_consensus::proofs::calculate_transaction_root(&next.body.transactions);
        Self::execute(self.factory, self.chain, parent, next.try_into_recovered().unwrap())
    }

    /// Submits a resealed header through the real BasicEngineValidator and Engine API tree.
    pub fn submit(&self, commitment: B256, deferred: bool) -> alloy_rpc_types_engine::PayloadStatus {
        let runtime = Runtime::test();
        let provider = BlockchainProvider::new(self.factory.clone()).unwrap();
        let overlay = OverlayManager::new(runtime.state_trie_overlay_worker_pool());
        let config = TreeConfig::default();
        let evm = BaseEvmConfig::new(self.chain.clone());
        let consensus = Arc::new(BaseBeaconConsensus::new(self.chain.clone()));
        let activation = if deferred && std::env::var("BASE_DEFERRED_TEST_WIRING").as_deref() != Ok("legacy") { Some(2) } else { None };
        let state = Arc::new(DeferredStateRootState { activation, ..Default::default() });
        let inner = BasicEngineValidator::new(provider.clone(), consensus.clone(), evm.clone(),
            BaseEngineValidator::<BaseTxEnvelope, _>::new::<KeccakKeyHasher>(self.chain.clone()),
            config.clone(), Box::new(NoopInvalidBlockHook::default()), overlay.clone(), runtime.clone())
            .with_state_root_strategy(Arc::new(DeferredStateRootStrategy { provider: provider.clone(), overlay: overlay.clone(), state: state.clone(), stock: Default::default() }));
        let validator = DeferredEngineValidator { inner, state: state.clone() };
        let (persistence, _rx) = mpsc::channel(); let (payloads, _prx) = unbounded_channel();
        let (sender, _events) = EngineApiTreeHandler::spawn_new(provider, consensus, validator,
            PersistenceHandle::new(persistence), PayloadBuilderHandle::<BasePayloadTypes>::new(payloads),
            CanonicalInMemoryState::with_head(self.parent.clone(), None, None), overlay, config,
            EngineApiKind::OpStack, evm, runtime.clone());
        let mut block = self.block.clone().into_block(); block.header.state_root = commitment;
        let payload = BasePayloadTypes::<BasePrimitives>::block_to_payload(block.seal_slow(), None);
        let (tx, rx) = oneshot::channel();
        sender.send(FromEngine::Request(EngineApiRequest::Beacon(BeaconEngineMessage::NewPayload { cause: tracing::Span::none(), payload, tx }))).unwrap();
        let status = runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.expect("engine response bounded").unwrap().expect("no operational error in semantic fixture") });
        let (tx, rx) = oneshot::channel(); sender.send(FromEngine::Event(FromOrchestrator::Terminate { tx })).unwrap();
        runtime.handle().block_on(async { tokio::time::timeout(Duration::from_secs(10), rx).await.unwrap().unwrap(); });
        drop(sender);
        if let Some(manager) = state.manager.get() { manager.drain().expect("owned root worker drains after engine termination"); }
        if activation.is_none() { assert!(state.manager.get().is_none(), "off path allocates exactly zero root managers"); }
        runtime.shutdown_timeout(Duration::from_secs(10)); status
    }
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

/// c4 activation must commit to the independently computed parent, never the current state.
#[test]
fn c4_activation_parent_post_state() {
    let f = Fixture::new();
    assert_ne!(f.parent.state_root, f.root, "activation fixture must distinguish real pre-state from real post-state");
    let genesis_root = f.parent.state_root; let first_root = f.root;
    let first_header_root = f.block.state_root();
    let next = f.next();
    assert_ne!(first_root, next.root, "F and F+1 actual MPT roots must be distinct");
    assert_ne!(genesis_root, next.root, "F-1 and F+1 actual MPT roots must be distinct");
    assert_eq!(first_header_root, genesis_root, "first active header F must commit R(F-1), not R(F)");
    assert_eq!(next.block.state_root(), first_root, "active header F+1 must commit independent R(F), not delayed header F");
}
