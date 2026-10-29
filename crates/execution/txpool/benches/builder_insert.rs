//! Cost of a `base_insertValidatedTransactions` call on a builder, by batch size and by the share
//! of the batch the builder's pool already holds.
//!
//! Every mempool node forwards each transaction to every builder, so most of what a builder
//! receives is already pooled. The `dup_percent` axis sweeps that share; `0` is a fresh batch,
//! `75` a mostly redundant one and `100` a fully redundant one.
//!
//! The pool is the production [`BaseTransactionPool`] with the Base validator over an in-memory
//! state provider, so validation here is cheaper than against a node's database and the measured
//! validation share is a lower bound.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench builder_insert
//! ```

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxHash, TxKind, U256};
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
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_primitives_traits::Recovered;
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_tasks::Runtime;
use reth_transaction_pool::{
    Pool, PoolConfig, PoolTransaction, TransactionOrigin, TransactionPool,
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};

type Provider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type BenchPool = BaseTransactionPool<Provider, InMemoryBlobStore, BaseEvmConfig>;

/// Transactions per call, from a small forwarder batch to a large one.
const BATCH_SIZES: [usize; 3] = [4, 16, 64];

/// Share of each batch already in the builder's pool.
const DUP_PERCENTS: [usize; 3] = [0, 75, 100];

/// Wall clock the pool starts at, in Unix seconds.
const NOW_SECS: u64 = 1_700_000_000;

fn build_pool() -> (BenchPool, Provider) {
    let mut genesis = build_test_genesis_everest();
    genesis.config.chain_id = ChainConfig::mainnet().chain_id;
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let client = MockEthProvider::<BasePrimitives>::new()
        .with_chain_spec(Arc::clone(&chain_spec))
        .with_genesis_block();
    let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
    let blob_store = InMemoryBlobStore::default();
    let validator = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
        .no_shanghai()
        .no_cancun()
        .build_with_tasks(Runtime::test(), blob_store.clone())
        .map(|inner| {
            let validator =
                BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
            let header = alloy_consensus::Header { timestamp: NOW_SECS, ..Default::default() };
            validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
            validator
        });
    let ordering = BaseOrdering::default();
    let pool = Pool::new(validator, ordering.clone(), blob_store, PoolConfig::default());
    (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
}

/// A funded sender's first transaction, as the pool and the wire carry it.
fn transaction(client: &Provider) -> (BasePooledTransaction, ValidatedTransaction<NoExtensions>) {
    let signer = PrivateKeySigner::random();
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
    let wire = ValidatedTransaction {
        sender: signer.address(),
        raw,
        metering: None,
        extensions: NoExtensions::default(),
    };
    (pooled, wire)
}

fn builder_insert(c: &mut Criterion) {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let _guard = runtime.enter();
    let mut group = c.benchmark_group("builder_insert");

    for batch_size in BATCH_SIZES {
        for dup_percent in DUP_PERCENTS {
            let dups = batch_size * dup_percent / 100;
            let (pool, client) = build_pool();
            let handler = BuilderApiImpl::new(pool.clone());

            let (pooled, mut batch): (Vec<_>, Vec<_>) =
                (0..dups).map(|_| transaction(&client)).unzip();
            let added =
                runtime.block_on(pool.add_transactions(TransactionOrigin::External, pooled));
            assert!(added.iter().all(Result::is_ok), "seeding the pool failed: {added:?}");

            let (fresh, fresh_wire): (Vec<_>, Vec<_>) =
                (dups..batch_size).map(|_| transaction(&client)).unzip();
            let fresh_hashes: Vec<TxHash> =
                fresh.iter().map(|tx: &BasePooledTransaction| *tx.hash()).collect();
            batch.extend(fresh_wire);

            let accepted = runtime
                .block_on(handler.insert_validated_transactions(batch.clone()))
                .expect("batch call");
            assert_eq!(
                accepted.iter().filter(|result| result.is_none()).count(),
                fresh_hashes.len(),
                "fresh transactions must be accepted and duplicates rejected",
            );

            group.bench_with_input(
                BenchmarkId::new(format!("batch_{batch_size}"), format!("dup_{dup_percent}")),
                &batch,
                |b, batch| {
                    b.iter_batched(
                        || {
                            // Fresh transactions must be new to the pool on every call.
                            pool.remove_transactions(fresh_hashes.clone());
                            batch.clone()
                        },
                        |batch| {
                            black_box(
                                runtime.block_on(handler.insert_validated_transactions(batch)),
                            )
                        },
                        BatchSize::PerIteration,
                    );
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, builder_insert);
criterion_main!(benches);
