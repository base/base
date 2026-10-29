//! Per-build cost of `best_transactions` on a large `BaseTransactionPool`.
//!
//! The builder creates one merged best-transactions iterator per flashblock (about 10 per block)
//! and per native block after Denim. Each arm measures that path with reth's pending and queued
//! subpools and the validity pool filled to the order of their configured limits:
//!
//! - `snapshot`: creating the iterator, which includes the cross-store lane bookkeeping the
//!   merge needs (validity lanes riding a standard run, standard transactions queued behind a
//!   validity run).
//! - `first_300`: the snapshot plus yielding a block's worth of candidates.
//!
//! The `isolated` pool has no sender with transactions in two stores; `shared` adds 1 000
//! senders whose standard transaction is queued right behind their validity transaction, which
//! is the case the merge has to bridge.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench best_transactions
//! ```

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{BasePrimitives, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, GuardLimits, ValidityOperator, ValidityPredicate,
};
use base_test_utils::build_test_genesis_everest;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_tasks::Runtime;
use reth_transaction_pool::{
    Pool, PoolConfig, SubPoolLimit, TransactionOrigin, TransactionPool,
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};

type Client = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
type BenchPool = BaseTransactionPool<Client, InMemoryBlobStore, BaseEvmConfig>;

/// Standard transactions pending in reth, one per sender.
const PENDING_STANDARD: usize = 10_000;
/// Standard transactions queued in reth behind a nonce gap; also the queued subpool limit.
const QUEUED_STANDARD: usize = 20_000;
/// Resting validity transactions, one per sender.
const VALIDITY: usize = 25_000;
/// Senders holding a validity transaction with a standard one queued behind it.
const SHARED_LANES: usize = 1_000;
/// Candidates a block's worth of selection pulls from the iterator.
const BLOCK_CANDIDATES: usize = 300;

const ONE_ETHER: u64 = 1_000_000_000_000_000_000;
const MIB: usize = 1024 * 1024;

const fn chain_id() -> u64 {
    ChainConfig::mainnet().chain_id
}

fn build_pool() -> (BenchPool, Client) {
    let mut genesis = build_test_genesis_everest();
    genesis.config.chain_id = chain_id();
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
            let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
            validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
            validator
        });
    let config = PoolConfig {
        pending_limit: SubPoolLimit::new(100_000, 125 * MIB),
        queued_limit: SubPoolLimit::new(QUEUED_STANDARD, 25 * MIB),
        ..PoolConfig::default()
    };
    let ordering = BaseOrdering::default();
    let pool = Pool::new(validator, ordering.clone(), blob_store, config);
    (BaseTransactionPool::new(pool, ordering).with_guard_limits(GuardLimits::default()), client)
}

fn funded_signer(client: &Client) -> PrivateKeySigner {
    let signer = PrivateKeySigner::random();
    client.add_account(signer.address(), ExtendedAccount::new(0, U256::from(ONE_ETHER)));
    signer
}

fn standard(signer: &PrivateKeySigner, nonce: u64, tip: u128) -> BasePooledTransaction {
    let tx = TxEip1559 {
        chain_id: chain_id(),
        nonce,
        gas_limit: 50_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: tip,
        to: TxKind::Call(Address::repeat_byte(0xEE)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
    let recovered = envelope.clone().try_into_recovered().unwrap();
    BasePooledTransaction::new(recovered, envelope.encode_2718_len())
}

/// A validity transaction whose predicate holds for the next thousand blocks, so it rests in
/// the validity pool for the whole measurement.
fn validity(signer: &PrivateKeySigner, nonce: u64, tip: u128) -> BasePooledTransaction {
    standard(signer, nonce, tip).with_validity_predicates(vec![ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(1_000),
    }])
}

async fn add(pool: &BenchPool, transactions: Vec<BasePooledTransaction>) {
    for transaction in transactions {
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
    }
}

/// A pool filled to the sizes above. `shared_lanes` senders hold both a resting validity
/// transaction at nonce 0 and a standard transaction at nonce 1, which reth queues behind the gap.
///
/// The runtime hosting the validation tasks is returned with the pool so it outlives the
/// measurement.
fn large_pool(shared_lanes: usize) -> (BenchPool, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let pool = runtime.block_on(async {
        let (pool, client) = build_pool();
        let pending: Vec<_> = (0..PENDING_STANDARD)
            .map(|index| standard(&funded_signer(&client), 0, 1 + index as u128 % 50))
            .collect();
        add(&pool, pending).await;
        let queued: Vec<_> = (0..QUEUED_STANDARD - shared_lanes)
            .map(|index| standard(&funded_signer(&client), 1, 1 + index as u128 % 50))
            .collect();
        add(&pool, queued).await;
        let resting: Vec<_> = (0..VALIDITY - shared_lanes)
            .map(|index| validity(&funded_signer(&client), 0, 1 + index as u128 % 50))
            .collect();
        add(&pool, resting).await;
        let mut shared = Vec::with_capacity(shared_lanes * 2);
        for index in 0..shared_lanes {
            let signer = funded_signer(&client);
            shared.push(validity(&signer, 0, 1 + index as u128 % 50));
            shared.push(standard(&signer, 1, 60));
        }
        add(&pool, shared).await;
        pool
    });
    let size = pool.pool_size();
    assert_eq!(size.pending, PENDING_STANDARD + VALIDITY, "validity transactions are pending");
    assert_eq!(size.queued, QUEUED_STANDARD, "standard successors are queued");
    (pool, runtime)
}

fn best_transactions_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("txpool/best_transactions");
    group.sample_size(20);
    for (name, shared_lanes) in [("isolated", 0), ("shared", SHARED_LANES)] {
        let (pool, _runtime) = large_pool(shared_lanes);
        group.bench_with_input(BenchmarkId::new("snapshot", name), &pool, |b, pool| {
            b.iter(|| black_box(pool.best_transactions()));
        });
        group.bench_with_input(BenchmarkId::new("first_300", name), &pool, |b, pool| {
            b.iter(|| {
                let best = pool.best_transactions();
                assert_eq!(best.take(BLOCK_CANDIDATES).map(black_box).count(), BLOCK_CANDIDATES);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, best_transactions_benches);
criterion_main!(benches);
