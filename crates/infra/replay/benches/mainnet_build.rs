//! Mainnet inputs through the native full-block payload builder, not the Flashblocks builder.
//!
//! Witness traversal only records flat reads during untimed preparation. Missing replay reads
//! fail. State-root computation is explicitly mocked to zero and the withdrawals root to its
//! captured value; production state-key hashing and full-block assembly still run.
//! The hand-written database records stateful read-through accesses that automock cannot model.
//! The root boundary implements external provider traits and rejects every unexpected read.

use std::{
    cell::RefCell, collections::BTreeMap, fmt::Debug, hint::black_box, io, sync::Arc, time::Instant,
};

use alloy_consensus::Transaction;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256};
use base_common_consensus::{BaseTransactionSigned, Predeploys};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BaseBuiltPayload, BasePayloadBuilderAttributes, ParkableBestPayloadTransactions,
    builder::{BasePayloadBuilderCtx, Builder, ExecutionInfo},
    config::BaseBuilderConfig,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkableBestTransactions, ParkedBestTransactions,
};
use base_proof_executor::TrieDB;
use base_replay::{ExecutionWitness, Fixture};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use reth_basic_payload_builder::{BuildOutcomeKind, PayloadConfig};
use reth_evm::execute::BlockBuilder;
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::{Account, Recovered, SealedHeader};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, ProviderError,
    StateProofProvider, StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, KeccakKeyHasher,
    MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};
use revm::{
    Database,
    bytecode::Bytecode,
    database::{BundleState, State, states::bundle_state::BundleRetention},
    state::AccountInfo,
};

/// Native full-block builder context.
pub type Context = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;
/// Production pending pool ordered to reproduce canonical transaction order.
pub type Pool = PendingPool<BaseOrdering<BasePooledTransaction>>;

/// Explicit mocked root boundary. Execution reads must use the captured database instead.
#[derive(Debug)]
pub struct MockRoots {
    withdrawals_root: B256,
}

impl StateRootProvider for MockRoots {
    fn state_root(&self, _: HashedPostState) -> Result<B256, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn state_root_from_nodes(&self, _: TrieInput) -> Result<B256, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn state_root_with_updates(
        &self,
        _: HashedPostState,
    ) -> Result<(B256, TrieUpdates), ProviderError> {
        Ok((B256::ZERO, TrieUpdates::default()))
    }
    fn state_root_from_nodes_with_updates(
        &self,
        _: TrieInput,
    ) -> Result<(B256, TrieUpdates), ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl HashedPostStateProvider for MockRoots {
    fn hashed_post_state(&self, bundle: &BundleState) -> Result<HashedPostState, ProviderError> {
        Ok(HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state()))
    }
}

impl StorageRootProvider for MockRoots {
    fn storage_root(&self, address: Address, _: HashedStorage) -> Result<B256, ProviderError> {
        assert_eq!(address, Predeploys::L2_TO_L1_MESSAGE_PASSER);
        Ok(self.withdrawals_root)
    }
    fn storage_proof(
        &self,
        _: Address,
        _: B256,
        _: HashedStorage,
    ) -> Result<StorageProof, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn storage_multiproof(
        &self,
        _: Address,
        _: &[B256],
        _: HashedStorage,
    ) -> Result<StorageMultiProof, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl AccountReader for MockRoots {
    fn basic_account(&self, _: &Address) -> Result<Option<Account>, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}
impl BytecodeReader for MockRoots {
    fn bytecode_by_hash(
        &self,
        _: &B256,
    ) -> Result<Option<reth_primitives_traits::Bytecode>, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}
impl BlockHashReader for MockRoots {
    fn block_hash(&self, _: u64) -> Result<Option<B256>, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn canonical_hashes_range(&self, _: u64, _: u64) -> Result<Vec<B256>, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}
impl StateProofProvider for MockRoots {
    fn proof(&self, _: TrieInput, _: Address, _: &[B256]) -> Result<AccountProof, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn multiproof(&self, _: TrieInput, _: MultiProofTargets) -> Result<MultiProof, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
    fn witness(
        &self,
        _: TrieInput,
        _: HashedPostState,
        _: ExecutionWitnessMode,
    ) -> Result<Vec<Bytes>, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}
impl StateProvider for MockRoots {
    fn storage(&self, _: Address, _: B256) -> Result<Option<U256>, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}

/// Captured prestate reads. Only the preparation pass has a witness-backed source.
#[derive(Debug)]
pub struct CapturedDb<'a> {
    source: RefCell<Option<TrieDB<&'a ExecutionWitness, &'a ExecutionWitness>>>,
    accounts: RefCell<BTreeMap<Address, Option<AccountInfo>>>,
    storage: RefCell<BTreeMap<(Address, U256), U256>>,
    code: RefCell<BTreeMap<B256, Bytecode>>,
    hashes: RefCell<BTreeMap<u64, B256>>,
}

impl<'a> CapturedDb<'a> {
    /// Fetches missing reads only during preparation; records absence separately from a miss.
    pub fn read<K: Copy + Ord + Debug, V: Clone>(
        &self,
        values: &RefCell<BTreeMap<K, V>>,
        key: K,
        fetch: impl FnOnce(
            &mut TrieDB<&'a ExecutionWitness, &'a ExecutionWitness>,
        ) -> Result<V, ProviderError>,
    ) -> Result<V, ProviderError> {
        if let Some(value) = values.borrow().get(&key) {
            return Ok(value.clone());
        }
        let mut source = self.source.borrow_mut();
        let source = source.as_mut().ok_or_else(|| {
            ProviderError::other(io::Error::other(format!("uncaptured database read: {key:?}")))
        })?;
        let value = fetch(source)?;
        values.borrow_mut().insert(key, value.clone());
        Ok(value)
    }
}

impl Database for &CapturedDb<'_> {
    type Error = ProviderError;
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.read(&self.accounts, address, |db| db.basic(address).map_err(ProviderError::other))
    }
    fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Self::Error> {
        self.read(&self.storage, (address, slot), |db| {
            db.storage(address, slot).map_err(ProviderError::other)
        })
    }
    fn code_by_hash(&mut self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.read(&self.code, hash, |db| db.code_by_hash(hash).map_err(ProviderError::other))
    }
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.read(&self.hashes, number, |db| db.block_hash(number).map_err(ProviderError::other))
    }
}

/// Prepares native attributes and the FIFO pool from a complete captured block.
pub fn prepare(fixture: &Fixture) -> (Context, Pool) {
    let chain_spec = Arc::new(BaseChainSpec::mainnet());
    let parent = Arc::new(SealedHeader::seal_slow(fixture.parent.clone()));
    let mut attributes = BasePayloadBuilderAttributes::<BaseTransactionSigned>::try_new(
        parent.hash(),
        fixture.execution_payload().unwrap(),
        3,
    )
    .unwrap();
    attributes.no_tx_pool = false;
    let deposit_count =
        attributes.transactions.iter().take_while(|tx| tx.value().is_deposit()).count();
    assert!(
        attributes.transactions[deposit_count..].iter().all(|tx| !tx.value().is_deposit()),
        "only leading deposits are supported"
    );
    let users = attributes.transactions.split_off(deposit_count);
    let mut pool = Pool::new(BaseOrdering::timestamp());
    let mut senders = BTreeMap::new();
    for (index, tx) in users.into_iter().enumerate() {
        let captured = &fixture.transactions[index + deposit_count];
        let sender_id = senders.len() as u64;
        let sender_id = *senders.entry(captured.sender).or_insert(sender_id);
        let transaction = BasePooledTransaction::new_with_received_at(
            Recovered::new_unchecked(tx.value().clone(), captured.sender),
            captured.encoded.len(),
            index as u128,
        );
        pool.add_transaction(
            Arc::new(ValidPoolTransaction {
                transaction_id: TransactionId::new(sender_id.into(), transaction.nonce()),
                transaction,
                propagate: false,
                timestamp: Instant::now(),
                origin: TransactionOrigin::External,
                authority_ids: None,
            }),
            fixture.header.base_fee_per_gas.unwrap(),
        );
    }
    let id = attributes.payload_attributes.id;
    let ctx = Context {
        evm_config: BaseEvmConfig::base(Arc::clone(&chain_spec)),
        chain_spec,
        config: PayloadConfig::new(parent, attributes, id),
        cancel: Default::default(),
        best_payload: None,
        builder_config: BaseBuilderConfig::default(),
    };
    (ctx, pool)
}

/// Native selection and EVM execution over a complete block, without finishing the payload.
pub fn execute<'a, 'w>(
    ctx: &Context,
    pool: &Pool,
    db: &'a CapturedDb<'w>,
    base_fee: u64,
) -> (State<&'a CapturedDb<'w>>, ExecutionInfo) {
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    state.load_cache_account(Predeploys::L1_BLOCK_INFO).unwrap();
    let mut builder = ctx.block_builder(&mut state).unwrap();
    builder.apply_pre_execution_changes().unwrap();
    let mut info = ctx.execute_sequencer_transactions(&mut builder).unwrap();
    let mut best = pool.best();
    best.no_updates();
    let txs = ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        best,
        BaseOrdering::timestamp(),
        base_fee,
    )));
    assert!(ctx.execute_best_transactions(&mut info, &mut builder, txs).unwrap().is_none());
    drop(builder);
    (state, info)
}

/// Calls the actual native full-block builder including its finish/assembly path.
pub fn build(
    ctx: &Context,
    pool: &Pool,
    db: &CapturedDb<'_>,
    roots: &MockRoots,
    base_fee: u64,
) -> BaseBuiltPayload {
    let ctx = Context {
        evm_config: ctx.evm_config.clone(),
        chain_spec: Arc::clone(&ctx.chain_spec),
        config: ctx.config.clone(),
        builder_config: ctx.builder_config.clone(),
        cancel: Default::default(),
        best_payload: None,
    };
    let outcome = Builder::new(|_| {
        let mut best = pool.best();
        best.no_updates();
        ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
            best,
            BaseOrdering::timestamp(),
            base_fee,
        )))
    })
    .build(db, roots, None, ctx)
    .unwrap();
    match outcome {
        BuildOutcomeKind::Better { payload } | BuildOutcomeKind::Freeze(payload) => payload,
        _ => panic!("complete captured block must produce a payload"),
    }
}

/// Checks mainnet commitments before measuring only full-block workloads.
pub fn mainnet_build(c: &mut Criterion) {
    let path = std::env::var("BASE_REPLAY_FIXTURE").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mainnet-execution.json.zst").into()
    });
    let fixture = Fixture::read(path).unwrap();
    fixture.validate().unwrap();
    let (ctx, pool) = prepare(&fixture);
    let base_fee = fixture.header.base_fee_per_gas.unwrap();
    let witness = fixture.execution.as_ref().expect("execution witness required");
    let db = CapturedDb {
        source: RefCell::new(Some(TrieDB::new(
            alloy_consensus::Sealed::new(fixture.parent.clone()),
            witness,
            witness,
        ))),
        accounts: Default::default(),
        storage: Default::default(),
        code: Default::default(),
        hashes: Default::default(),
    };
    let roots = MockRoots { withdrawals_root: fixture.header.withdrawals_root.unwrap() };
    let _ = build(&ctx, &pool, &db, &roots, base_fee);
    drop(db.source.borrow_mut().take());
    let payload = build(&ctx, &pool, &db, &roots, base_fee);
    let mut expected = fixture.header.clone();
    expected.state_root = B256::ZERO; // Explicit mock, not a valid block for submission.
    assert_eq!(payload.block().header(), &expected);
    assert_eq!(
        payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| Bytes::from(tx.encoded_2718()))
            .collect::<Vec<_>>(),
        fixture.transactions.iter().map(|tx| tx.encoded.clone()).collect::<Vec<_>>()
    );
    let (mut executed, info) = execute(&ctx, &pool, &db, base_fee);
    assert_eq!(info.cumulative_gas_used, fixture.header.gas_used);
    let transitions = executed.transition_state.clone();
    executed.merge_transitions(BundleRetention::Reverts);
    let canonical = payload.executed_block().unwrap();
    assert_eq!(executed.bundle_state.state, canonical.execution_output.state.state);
    assert_eq!(roots.hashed_post_state(&executed.bundle_state).unwrap(), *canonical.hashed_state);
    let mut best = pool.best();
    best.no_updates();
    let mut selected = ParkedBestTransactions::new(best, BaseOrdering::timestamp(), base_fee);
    for expected in &fixture.transactions[ctx.config.attributes.transactions.len()..] {
        let actual = selected.next().expect("every captured pool transaction is eligible");
        assert_eq!(*actual.hash(), expected.hash);
        selected.mark_committed(&actual);
    }
    assert!(selected.next().is_none());
    let first = fixture.transactions[0].sender;
    let account = db.accounts.borrow_mut().remove(&first).unwrap();
    assert!((&db).basic(first).is_err());
    db.accounts.borrow_mut().insert(first, account);
    eprintln!(
        "native full-block builder: block={}, txs={}, gas={}; FIFO/flat-state, roots mocked, network excluded",
        fixture.header.number,
        fixture.transactions.len(),
        info.cumulative_gas_used
    );
    let mut group = c.benchmark_group(format!("sequencer/full-block/{}", fixture.header.number));
    group.sample_size(10);
    group.throughput(Throughput::Elements(fixture.transactions.len() as u64));
    group.bench_function("build-mocked-roots", |b| {
        b.iter(|| black_box(build(&ctx, &pool, &db, &roots, base_fee)))
    });
    group.bench_function("select-execute-flat-state", |b| {
        b.iter(|| black_box(execute(&ctx, &pool, &db, base_fee)))
    });
    group.bench_function("pool-select-commit", |b| {
        b.iter(|| {
            let mut best = pool.best();
            best.no_updates();
            let mut best = ParkedBestTransactions::new(best, BaseOrdering::timestamp(), base_fee);
            while let Some(tx) = best.next() {
                best.mark_committed(black_box(&tx));
            }
        })
    });
    group.bench_function("transition-merge", |b| {
        b.iter_batched(
            || {
                let mut state = State::builder().with_database(&db).with_bundle_update().build();
                state.transition_state = transitions.clone();
                state
            },
            |mut state| {
                state.merge_transitions(BundleRetention::Reverts);
                black_box(state.take_bundle())
            },
            BatchSize::SmallInput,
        )
    });
    group.bench_function("state-key-hashing", |b| {
        b.iter(|| black_box(roots.hashed_post_state(&executed.bundle_state).unwrap()))
    });
    group.finish();
}

criterion_group!(benches, mainnet_build);
criterion_main!(benches);
