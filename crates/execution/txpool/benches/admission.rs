//! Benchmark: same-nonce replacement detection on the protocol pool.
//!
//! On every protocol/validity/EIP-8130 admission, `protocol_replacement_hash`
//! (see `pool.rs`) looks for an existing same-sender/same-nonce transaction to
//! replace. This bench measures that production function directly against a real
//! [`BaseTransactionPool`] populated with a single sender holding `K` pooled
//! transactions, sweeping `K` to expose how lookup cost scales with the sender's
//! queue depth.
//!
//! The function currently scans the sender's full transaction set (`O(K)` work
//! and allocation); a stacked follow-up swaps its body to an indexed lookup that
//! stays flat in `K`. Because the bench targets `protocol_replacement_hash`
//! itself rather than a copy, CI's historical comparison shows the improvement
//! land on the real code path across the two-PR stack.

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_chains::ChainConfig;
use base_common_consensus::{BasePrimitives, BaseTxEnvelope};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator,
};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_tasks::Runtime;
use reth_transaction_pool::{
    Pool, PoolConfig, TransactionOrigin, TransactionPool, blobstore::InMemoryBlobStore,
    validate::EthTransactionValidatorBuilder,
};
use tokio::runtime::Runtime as TokioRuntime;

/// Sender queue depths swept by both strategies.
const QUEUE_DEPTHS: [u64; 4] = [1, 8, 32, 128];

/// The reth pool type produced by [`build_pool`], mirroring the production
/// `BaseTransactionPool` instantiation in `pool.rs`.
type BenchPool = BaseTransactionPool<
    MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
    InMemoryBlobStore,
    BaseEvmConfig,
>;

/// A deterministic signer so the populated pool is identical across runs.
fn bench_signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).expect("valid secp256k1 key")
}

/// Builds a `BaseTransactionPool` over a mock provider, mirroring the crate's
/// `build_integration_pool` test fixture but with a large per-account slot cap so
/// a single sender can hold a deep pending queue.
fn build_pool() -> (BenchPool, MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>) {
    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().cobalt_activated().build());
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
            BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
                .require_l1_data_gas_fee(false)
        });
    let ordering = BaseOrdering::default();
    // A generous account-slot cap so the deepest swept queue fits in one sender.
    let config = PoolConfig { max_account_slots: 8192, ..PoolConfig::default() };
    let pool = Pool::new(validator, ordering.clone(), blob_store, config);
    (BaseTransactionPool::new(pool, ordering), client)
}

/// A signed, self-paying EIP-1559 transaction at `nonce` for `signer`. Standard
/// EIP-1559 transactions are not guard-gated, so a single sender can pool a deep
/// contiguous nonce queue.
fn signed_1559(signer: &PrivateKeySigner, nonce: u64) -> BasePooledTransaction {
    let tx = TxEip1559 {
        chain_id: ChainConfig::mainnet().chain_id,
        nonce,
        gas_limit: 50_000,
        max_fee_per_gas: 1_000,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(Address::repeat_byte(0xEE)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("sign");
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
    let recovered = envelope.clone().try_into_recovered().expect("recover");
    BasePooledTransaction::new(recovered, envelope.encode_2718_len())
}

/// Populates a fresh pool with `depth` contiguous-nonce transactions from a
/// single funded sender and returns it alongside that sender's address.
fn populated_pool(rt: &TokioRuntime, depth: u64) -> (BenchPool, Address) {
    let signer = bench_signer();
    let sender = signer.address();
    let pool = rt.block_on(async {
        let (pool, client) = build_pool();
        client.add_account(sender, ExtendedAccount::new(0, U256::from(u128::MAX)));
        for nonce in 0..depth {
            pool.add_transaction(TransactionOrigin::Local, signed_1559(&signer, nonce))
                .await
                .expect("admit");
        }
        pool
    });
    (pool, sender)
}

fn bench_replacement_lookup(c: &mut Criterion) {
    // Building and populating the pool spawns validation tasks, so it must run
    // inside a Tokio runtime; the measured lookups themselves are synchronous.
    let rt = TokioRuntime::new().expect("tokio runtime");

    let mut group = c.benchmark_group("admission/replacement_lookup");
    for depth in QUEUE_DEPTHS {
        let (pool, sender) = populated_pool(&rt, depth);
        // Query a nonce in the middle of the queue: a representative hit the
        // lookup must find without short-circuiting at either end.
        let target = depth / 2;
        group.throughput(Throughput::Elements(depth));

        group.bench_with_input(
            BenchmarkId::new("protocol_replacement_hash", depth),
            &pool,
            |b, pool| {
                b.iter(|| {
                    black_box(pool.protocol_replacement_hash(black_box(sender), black_box(target)))
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_replacement_lookup);
criterion_main!(benches);
