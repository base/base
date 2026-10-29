//! End-to-end benchmark of the native payload builder's selection loop across consecutive blocks.
//!
//! Each iteration builds [`BLOCKS`] one-shot blocks, the post-Denim shape, through
//! `BasePayloadBuilderCtx::execute_best_transactions`. The pool holds `resting` validity
//! transactions whose balance predicates never become satisfied, plus enough plain transfers to
//! fill every block. Transaction events are enabled, as in production.
//!
//! With resting predicates off, every block re-evaluates every resting transaction. With them
//! enforced, only the first block evaluates them and later blocks hold them back unevaluated.
//! Every block builds on the same parent state, so the carried set stays valid from block to
//! block.

use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use base_common_chains::BaseUpgrade;
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope, Predeploys};
use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD,
    ParkableBestPayloadTransactions, REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL,
    RejectionCache, RestingPredicateMode, RestingPredicates,
    builder::{BasePayloadBuilderCtx, ExecutionInfo},
    config::BaseBuilderConfig,
    payload::EthPayloadBuilderAttributes,
};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use criterion::{Criterion, criterion_group, criterion_main};
use reth_basic_payload_builder::PayloadConfig;
use reth_ethereum_forks::ForkCondition;
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_builder::PayloadId;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::db::State;
use reth_transaction_pool::{
    TransactionOrigin, ValidPoolTransaction, identifier::TransactionId, pool::PendingPool,
};
use revm::{
    database::{InMemoryDB, TransitionState},
    state::AccountInfo,
};

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;
type Ctx = BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>;

const CHAIN_ID: u64 = 8_453;
/// Consecutive blocks built per iteration.
const BLOCKS: u64 = 10;
/// Plain transfers that fill each block.
const FILLER_PER_BLOCK: usize = 120;
/// Resting validity transaction counts to measure.
const RESTING_COUNTS: [usize; 2] = [20_000, 40_000];
/// Rejection-cache populations to measure: empty, and the order of magnitude a running builder
/// holds from rejections in earlier blocks.
const REJECTED_COUNTS: [usize; 2] = [0, 10_000];
const TRANSFER_GAS: u64 = 21_000;
/// Sender indexes start here so they do not collide with the watched predicate addresses.
const SENDER_OFFSET: usize = 1_000_000;
const DENIM_TIMESTAMP: u64 = 1;

fn address(index: usize) -> Address {
    Address::from_word(U256::from(index + 1).into())
}

/// Builds an executable transfer. Resting transactions get a higher tip so every block
/// considers them before the filler.
fn transaction(
    sender_index: usize,
    priority_fee: u128,
    predicates: Vec<ValidityPredicate>,
) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: TRANSFER_GAS,
        max_fee_per_gas: 1_000_000_000_000,
        max_priority_fee_per_gas: priority_fee,
        to: TxKind::Call(Address::repeat_byte(0xee)),
        // Distinguishes otherwise identical transfers from different senders.
        value: U256::from(sender_index),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let sender = address(SENDER_OFFSET + sender_index);
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender),
        encoded_length,
    )
    .with_validity_predicates(predicates);

    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new(((SENDER_OFFSET + sender_index) as u64).into(), 0),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// A pool of `resting` never-satisfied validity transactions, each watching its own balance, plus
/// one block of filler, and the parent state that funds every sender.
fn resting_pool(resting: usize) -> (Pool, InMemoryDB) {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut db = InMemoryDB::default();
    for sender_index in 0..resting + FILLER_PER_BLOCK {
        let (priority_fee, predicates) = if sender_index < resting {
            (
                10,
                vec![ValidityPredicate::Balance {
                    address: address(sender_index),
                    op: ValidityOperator::Equal,
                    value: U256::ONE,
                }],
            )
        } else {
            (1, Vec::new())
        };
        pool.add_transaction(transaction(sender_index, priority_fee, predicates), 0);
        db.insert_account_info(
            address(SENDER_OFFSET + sender_index),
            AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
        );
    }
    (pool, db)
}

fn parkable(pool: &Pool) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        pool.best(),
        Ordering::coinbase_tip(),
        0,
    )))
}

/// A rejection cache with the builder's default sizing, holding `rejected` hashes of transactions
/// that are not in the pool, as a running builder accumulates from earlier blocks.
fn populated_rejection_cache(rejected: usize) -> RejectionCache {
    let cache = RejectionCache::new(REJECTION_CACHE_MAX_CAPACITY, REJECTION_CACHE_TTL);
    for index in 0..rejected {
        cache.insert(B256::from(U256::from(index) | (U256::from(1) << 255)));
    }
    cache.run_pending_tasks();
    cache
}

/// A Denim payload context whose gas limit fits exactly one block of filler.
fn payload_context(mode: RestingPredicateMode, rejection_cache: RejectionCache) -> Ctx {
    let chain_spec = Arc::new(
        BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(DENIM_TIMESTAMP))
            .build(),
    );
    let gas_limit = FILLER_PER_BLOCK as u64 * TRANSFER_GAS;
    let parent = Arc::new(SealedHeader::seal_slow(Header { gas_limit, ..Default::default() }));
    let payload_id = PayloadId::new([0; 8]);
    let attributes = BasePayloadBuilderAttributes::<BaseTxEnvelope> {
        payload_attributes: EthPayloadBuilderAttributes {
            id: payload_id,
            parent: parent.hash(),
            timestamp: DENIM_TIMESTAMP,
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        },
        gas_limit: Some(gas_limit),
        ..Default::default()
    };
    BasePayloadBuilderCtx {
        evm_config: BaseEvmConfig::base(Arc::clone(&chain_spec)),
        builder_config: BaseBuilderConfig {
            rejection_cache,
            ..BaseBuilderConfig::default().with_resting_predicate_mode(mode)
        },
        chain_spec,
        config: PayloadConfig::new(parent, attributes, payload_id),
        cancel: Default::default(),
        best_payload: None,
    }
}

/// Runs the selection loop for one block and returns the number of included transactions.
fn run_block(
    ctx: &Ctx,
    pool: &Pool,
    db: &InMemoryDB,
    resting: Option<&mut RestingPredicates>,
) -> usize {
    let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
    state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
    let mut builder = ctx.block_builder(&mut state).expect("block builder");
    let mut info = ExecutionInfo::default();
    black_box(
        ctx.execute_best_transactions(&mut info, &mut builder, parkable(pool), resting)
            .expect("in-memory execution cannot fail"),
    );
    builder.executor().receipts().len()
}

/// Builds [`BLOCKS`] consecutive blocks, carrying the resting set when `mode` is enabled.
fn run_blocks(ctx: &Ctx, pool: &Pool, db: &InMemoryDB, mode: RestingPredicateMode) {
    let mut resting = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
    let mut parent = B256::ZERO;
    for block in 1..=BLOCKS {
        // The production builder passes the pre-block writes; this loop makes none.
        resting.begin_block(parent, block, Some(&TransitionState::default()));
        let included = run_block(ctx, pool, db, mode.is_enabled().then_some(&mut resting));
        assert_eq!(included, FILLER_PER_BLOCK);
        parent = B256::from(U256::from(block));
        resting.end_block(parent);
    }
}

fn native_build_loop_benches(c: &mut Criterion) {
    let events = tempfile::tempdir().expect("temporary event directory");
    GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
        enabled: true,
        ..TransactionEventWriterConfig::disabled(
            TransactionEventProducer::BaseBuilder,
            "bench",
            events.path().join("events.jsonl"),
        )
    }))
    .expect("event writer initializes");

    let mut group = c.benchmark_group("native_build_loop/resting_predicates");
    group.sample_size(10).measurement_time(Duration::from_secs(20));
    for resting in RESTING_COUNTS {
        let (pool, db) = resting_pool(resting);
        for rejected in REJECTED_COUNTS {
            // The empty-cache case keeps its original name so earlier results stay comparable.
            let rejected_suffix =
                if rejected == 0 { String::new() } else { format!("/rejected={rejected}") };
            for mode in [RestingPredicateMode::Off, RestingPredicateMode::Enforce] {
                let ctx = payload_context(mode, populated_rejection_cache(rejected));
                group.bench_function(
                    format!("resting={resting}/blocks={BLOCKS}/mode={mode:?}{rejected_suffix}"),
                    |b| b.iter(|| run_blocks(&ctx, &pool, &db, mode)),
                );
            }
        }
    }
    group.finish();
}

criterion_group!(benches, native_build_loop_benches);
criterion_main!(benches);
