//! Deterministic instruction-count benchmark for a `base_insertValidatedTransactions` batch on a
//! builder.
//!
//! The `iai-callgrind` counterpart to `builder_insert.rs`, for its largest batch
//! ([`BATCH_SIZE`] transactions) with none (`fresh`) or three quarters (`mostly_duplicate`) of
//! the batch already pooled. Every mempool node forwards each transaction to every builder, so a
//! builder runs this once per forwarded batch, about once per submitted transaction per mempool
//! node (~10^8 submitted transactions/day), and most of what it receives is already pooled.
//!
//! The counted region is one [`BuilderApiServer::insert_validated_transactions`] call on the
//! production [`BuilderApiImpl`] over the production [`BaseTransactionPool`]: decoding the wire
//! bytes, answering duplicates from the pool, the Base validator over an in-memory state
//! provider, and pool insertion. Validation against an in-memory provider is cheaper than
//! against a node's database, so the validation share is a lower bound. No metrics recorder or
//! transaction-event writer is installed, so their cost is not counted.
//!
//! Production runs validation jobs on the blocking workers spawned by
//! `TransactionValidationTaskExecutor::spawn`. Callgrind only counts the thread that enters the
//! benchmark function, and work on other threads would make the count depend on scheduling, so
//! the setup builds the same executor with `TransactionValidationTaskExecutor::new` and spawns
//! its `ValidationTask` on a current-thread tokio runtime. The call is driven with
//! `Runtime::block_on`, so the job hand-off and the validation run on the measured thread.
//!
//! Signer keys are derived from their index, so the batch, and with it every hash and map
//! layout, is identical across runs. Building the pool, seeding the duplicates, and one warm-up
//! insert that pays one-time lazy initialization run in iai's unmeasured setup, and the inputs
//! are handed back so they drop outside the counted region.
//!
//! The teardown pins how many transactions the call inserted and how many it rejected as
//! duplicates, and the pool's size afterwards. They are fixed by the workload, so a mismatch
//! means the insert path now admits different transactions, and the instruction count is not
//! comparable with the base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, BuilderApiImpl, BuilderApiServer, GuardLimits, NoExtensions,
    ValidatedTransaction,
};
use base_test_utils::build_test_genesis_everest;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use jsonrpsee::types::ErrorObjectOwned;
use reth_primitives_traits::Recovered;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::{
    Pool, PoolConfig, TransactionOrigin, TransactionPool, TransactionValidationTaskExecutor,
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};
use tokio::runtime::Runtime;

type Provider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type BenchPool = BaseTransactionPool<Provider, InMemoryBlobStore, BaseEvmConfig>;
type WireTransaction = ValidatedTransaction<NoExtensions>;
type InsertResults = Vec<Option<ErrorObjectOwned>>;

/// Transactions per call: the largest batch `builder_insert.rs` measures.
const BATCH_SIZE: usize = 64;

/// Batch members already pooled in the `mostly_duplicate` case: three quarters of the batch.
const MOSTLY_DUPLICATE: usize = BATCH_SIZE * 3 / 4;

/// Wall clock the pool starts at, in Unix seconds.
const NOW_SECS: u64 = 1_700_000_000;

/// A builder ready to receive one batch, plus how many of the batch it already holds.
struct Input {
    runtime: Runtime,
    pool: BenchPool,
    handler: BuilderApiImpl<BenchPool>,
    batch: Vec<WireTransaction>,
    duplicates: usize,
}

/// Builds the production pool with its validation task on `runtime` instead of on spawned
/// blocking workers.
fn build_pool(runtime: &Runtime) -> (BenchPool, Provider) {
    let mut genesis = build_test_genesis_everest();
    genesis.config.chain_id = ChainConfig::mainnet().chain_id;
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let client = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let blob_store = InMemoryBlobStore::default();
    let inner = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
        .no_shanghai()
        .no_cancun()
        .build(blob_store.clone());
    let validator = BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
    let header = alloy_consensus::Header { timestamp: NOW_SECS, ..Default::default() };
    validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
    let (validator, validation_task) = TransactionValidationTaskExecutor::new(validator);
    runtime.spawn(validation_task.run());
    let ordering = BaseOrdering::default();
    let pool = Pool::new(validator, ordering.clone(), blob_store, PoolConfig::default());
    (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
}

/// The first transaction of a funded sender whose key is derived from `index`, as the pool and
/// the wire carry it.
fn transaction(client: &Provider, index: u64) -> (BasePooledTransaction, WireTransaction) {
    let signer = PrivateKeySigner::from_bytes(&B256::from(U256::from(index + 1))).expect("key");
    client.add_account(
        signer.address(),
        ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)),
    );
    let tx = TxEip1559 {
        chain_id: ChainConfig::mainnet().chain_id,
        nonce: 0,
        gas_limit: 50_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(Address::repeat_byte(0xEE)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("signs");
    let signed: BaseTransactionSigned = tx.into_signed(signature).into();
    let raw: Bytes = signed.encoded_2718().into();
    let pooled =
        BasePooledTransaction::new(Recovered::new_unchecked(signed, signer.address()), raw.len());
    let wire = WireTransaction {
        sender: signer.address(),
        raw,
        metering: None,
        extensions: NoExtensions::default(),
    };
    (pooled, wire)
}

/// Builds a builder whose pool already holds the first `duplicates` transactions of the batch,
/// outside the counted region.
fn setup(duplicates: usize) -> Input {
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let (pool, client) = build_pool(&runtime);
    let handler = BuilderApiImpl::new(pool.clone());

    // Index 0 is the warm-up sender; the batch uses indices 1..=BATCH_SIZE.
    let (_, warm_up) = transaction(&client, 0);
    let warmed = runtime
        .block_on(handler.insert_validated_transactions(vec![warm_up]))
        .expect("warm-up call");
    assert_eq!(warmed, vec![None], "warm-up transaction must be accepted");

    let (pooled, batch): (Vec<_>, Vec<_>) =
        (1..=BATCH_SIZE as u64).map(|index| transaction(&client, index)).unzip();
    let seeded = runtime.block_on(pool.add_transactions(
        TransactionOrigin::External,
        pooled.into_iter().take(duplicates).collect(),
    ));
    assert!(seeded.iter().all(Result::is_ok), "seeding the pool failed: {seeded:?}");

    Input { runtime, pool, handler, batch, duplicates }
}

/// Checks the call's outcomes against the workload's documented counts.
fn check_outcomes((results, input): (InsertResults, Input)) {
    let inserted = results.iter().filter(|result| result.is_none()).count();
    let duplicates = results.len() - inserted;
    let expected = match input.duplicates {
        0 => (64, 0),
        MOSTLY_DUPLICATE => (16, 48),
        other => unimplemented!("no documented outcome for {other} duplicates"),
    };
    assert_eq!(
        (inserted, duplicates),
        expected,
        "builder_insert duplicates={} admission changed: {results:?}",
        input.duplicates
    );
    assert_eq!(
        input.pool.pool_size().total,
        BATCH_SIZE + 1,
        "the pool holds the warm-up transaction and the whole batch"
    );
}

#[library_benchmark]
#[bench::fresh(args = (0), setup = setup, teardown = check_outcomes)]
#[bench::mostly_duplicate(args = (MOSTLY_DUPLICATE), setup = setup, teardown = check_outcomes)]
fn insert_batch(mut input: Input) -> (InsertResults, Input) {
    let batch = std::mem::take(&mut input.batch);
    let results = input
        .runtime
        .block_on(input.handler.insert_validated_transactions(batch))
        .expect("batch call");
    (black_box(results), input)
}

library_benchmark_group!(
    name = builder_insert;
    benchmarks = insert_batch
);

main!(library_benchmark_groups = builder_insert);
