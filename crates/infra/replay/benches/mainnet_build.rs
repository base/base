//! Mainnet inputs through the actual Flashblocks selection/execution and assembly code.
//!
//! The proof witness is used ONLY during untimed preparation to record flat database reads.
//! Timed runs cannot fall back to it: missing reads fail. FIFO is the production pool ordering
//! configured to reproduce canonical transaction order, not a reconstruction of the historical
//! mempool. State-root calculation and the message-passer storage root are explicitly excluded;
//! the latter is a fixed captured value. Publication measures JSON encoding, not network delivery.
//! The hand-written database is a read-through recorder during preparation and a strict replay
//! afterward; an automock cannot express that stateful generic read-through behavior.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt::Debug,
    hint::black_box,
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256, U256, address};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, BuilderConfig, ExecutionInfo,
    FlashblocksExtraCtx, ParkableBestPayloadTransactions, RejectionCache, ResourceLimits,
    test_utils::{build_block, execute_pre_steps},
};
use base_common_consensus::BaseTransactionSigned;
use base_common_flashblocks::FlashblockId;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_execution_txpool::{BaseOrdering, BasePooledTransaction, ParkedBestTransactions};
use base_proof_executor::TrieDB;
use base_replay::{ExecutionWitness, Fixture};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use reth_basic_payload_builder::PayloadConfig;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use reth_trie_common::{
    HashedPostState, HashedStorage, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};
use revm::{
    Database,
    bytecode::Bytecode,
    database::{BundleState, State},
    state::AccountInfo,
};

/// The real pending-pool iterator, with captured arrival order for deterministic replay.
pub type Pool = PendingPool<BaseOrdering<BasePooledTransaction>>;

/// Explicit storage boundary for assembly-only timings. Computing a state root is forbidden.
#[derive(Debug)]
pub struct ExcludedRoots {
    withdrawals_root: B256,
}

impl StateRootProvider for ExcludedRoots {
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
        Err(ProviderError::UnsupportedProvider)
    }
    fn state_root_from_nodes_with_updates(
        &self,
        _: TrieInput,
    ) -> Result<(B256, TrieUpdates), ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl HashedPostStateProvider for ExcludedRoots {
    fn hashed_post_state(&self, _: &BundleState) -> Result<HashedPostState, ProviderError> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl StorageRootProvider for ExcludedRoots {
    fn storage_root(&self, address: Address, _: HashedStorage) -> Result<B256, ProviderError> {
        assert_eq!(address, address!("4200000000000000000000000000000000000016"));
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

/// Captured prestate reads. Only the preparation pass has a witness-backed source.
#[derive(Debug)]
pub struct CapturedDb<'a> {
    source: RefCell<Option<TrieDB<&'a ExecutionWitness, &'a ExecutionWitness>>>,
    accounts: RefCell<BTreeMap<Address, Option<AccountInfo>>>,
    storage: RefCell<BTreeMap<(Address, U256), U256>>,
    code: RefCell<BTreeMap<B256, Bytecode>>,
    hashes: RefCell<BTreeMap<u64, B256>>,
    roots: ExcludedRoots,
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

impl AsRef<ExcludedRoots> for CapturedDb<'_> {
    fn as_ref(&self) -> &ExcludedRoots {
        &self.roots
    }
}

/// Prepares the real builder context and FIFO pool from a complete captured block.
pub fn prepare(fixture: &Fixture) -> (BasePayloadBuilderCtx, Pool) {
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
    let block_env_attributes = BaseNextBlockEnvAttributes {
        timestamp: fixture.header.timestamp,
        suggested_fee_recipient: fixture.header.beneficiary,
        prev_randao: fixture.header.mix_hash,
        gas_limit: fixture.header.gas_limit,
        parent_beacon_block_root: fixture.header.parent_beacon_block_root,
        extra_data: fixture.header.extra_data.clone(),
    };
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let evm_env = evm_config.next_evm_env(&parent, &block_env_attributes).unwrap();
    let id = attributes.payload_attributes.id;
    let ctx = BasePayloadBuilderCtx {
        evm_config,
        chain_spec,
        config: PayloadConfig::new(parent, attributes, id),
        evm_env,
        block_env_attributes,
        cancel: Default::default(),
        extra: FlashblocksExtraCtx::default(),
        builder_config: BuilderConfig::default(),
    };
    (ctx, pool)
}

/// Real pool selection and execution, excluding payload assembly and database population.
pub fn execute<'a, 'w>(
    ctx: &BasePayloadBuilderCtx,
    pool: &Pool,
    db: &'a CapturedDb<'w>,
) -> (State<&'a CapturedDb<'w>>, ExecutionInfo) {
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let mut info = execute_pre_steps(&mut state, ctx).unwrap();
    let mut best = pool.best();
    best.no_updates();
    let mut txs = BestFlashblocksTxs::new(
        ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
            best,
            BaseOrdering::timestamp(),
            ctx.evm_env.block_env.basefee,
        ))),
        RejectionCache::new(1_000, Duration::from_secs(60)),
    );
    let limits = ResourceLimits { block_gas_limit: ctx.block_gas_limit(), ..Default::default() };
    ctx.execute_best_transactions(
        &mut info,
        &mut BlockDeferrals::default(),
        &mut state,
        &mut txs,
        &limits,
    )
    .unwrap();
    (state, info)
}

/// Copies only assembly inputs, outside assembly-only timing.
pub fn assembly_info(info: &ExecutionInfo) -> ExecutionInfo {
    ExecutionInfo {
        executed_transactions: info.executed_transactions.clone(),
        executed_senders: info.executed_senders.clone(),
        receipts: info.receipts.clone(),
        cumulative_gas_used: info.cumulative_gas_used,
        total_fees: info.total_fees,
        da_footprint_scalar: info.da_footprint_scalar,
        ..Default::default()
    }
}

/// Runs checks before measuring production selection, assembly and publication encoding.
pub fn mainnet_build(c: &mut Criterion) {
    let path = std::env::var("BASE_REPLAY_FIXTURE").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mainnet-execution.json.zst").into()
    });
    let fixture = Fixture::read(path).unwrap();
    fixture.validate().unwrap();
    let (ctx, pool) = prepare(&fixture);
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
        roots: ExcludedRoots { withdrawals_root: fixture.header.withdrawals_root.unwrap() },
    };
    // Capture reads through the actual builder, then physically remove the proof database.
    let _ = execute(&ctx, &pool, &db);
    drop(db.source.borrow_mut().take());
    let (mut state, mut info) = execute(&ctx, &pool, &db);
    assert_eq!(info.cumulative_gas_used, fixture.header.gas_used);
    assert_eq!(info.executed_transactions.len(), fixture.transactions.len());
    let (payload, message, _) =
        build_block(&mut state, &ctx, &mut info, FlashblockId::default(), false).unwrap();
    let mut expected = fixture.header.clone();
    expected.state_root = B256::ZERO; // Deliberately uncomputed for intermediate Flashblocks.
    assert_eq!(payload.block().header(), &expected);
    assert_eq!(
        message.diff.transactions,
        fixture.transactions.iter().map(|tx| tx.encoded.clone()).collect::<Vec<_>>()
    );
    // A later Flashblock publishes only its suffix, but retains whole-block commitments.
    let tail_start = fixture.transactions.len() - fixture.transactions.len().div_ceil(10);
    info.extra.last_flashblock_index = tail_start;
    let (tail_payload, tail_message, _) =
        build_block(&mut state, &ctx, &mut info, FlashblockId::default(), false).unwrap();
    assert_eq!(tail_payload.block().header(), &expected);
    assert_eq!(tail_message.diff.transactions, message.diff.transactions[tail_start..]);
    // Negative control: a missing captured account must fail, not turn into an empty account.
    let first = fixture.transactions[0].sender;
    let account = db.accounts.borrow_mut().remove(&first).unwrap();
    assert!((&db).basic(first).is_err());
    db.accounts.borrow_mut().insert(first, account);
    eprintln!(
        "mainnet builder: block={}, txs={}, gas={}, captured_accounts={}, captured_slots={}; FIFO/flat-state, roots and network excluded",
        fixture.header.number,
        fixture.transactions.len(),
        info.cumulative_gas_used,
        db.accounts.borrow().len(),
        db.storage.borrow().len()
    );
    let (executed, info) = execute(&ctx, &pool, &db);
    let mut group = c.benchmark_group(format!("sequencer/mainnet/{}", fixture.header.number));
    group.sample_size(10);
    group.throughput(Throughput::Elements(fixture.transactions.len() as u64));
    group.bench_function("select-execute-flat-state", |b| {
        b.iter(|| black_box(execute(&ctx, &pool, &db)))
    });
    group.bench_function("build-no-roots", |b| {
        b.iter(|| {
            let (mut state, mut info) = execute(&ctx, &pool, &db);
            black_box(
                build_block(&mut state, &ctx, &mut info, FlashblockId::default(), false).unwrap(),
            )
        })
    });
    for (name, start) in [("assemble-no-roots", 0), ("assemble-tail-no-roots", tail_start)] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let mut state =
                        State::builder().with_database(&db).with_bundle_update().build();
                    state.transition_state = executed.transition_state.clone();
                    let mut info = assembly_info(&info);
                    info.extra.last_flashblock_index = start;
                    (state, info)
                },
                |(mut state, mut info)| {
                    black_box(
                        build_block(&mut state, &ctx, &mut info, FlashblockId::default(), false)
                            .unwrap(),
                    )
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.bench_function("publication-json", |b| {
        b.iter(|| black_box(serde_json::to_vec(&message).unwrap()))
    });
    group.finish();
}

criterion_group!(benches, mainnet_build);
criterion_main!(benches);
