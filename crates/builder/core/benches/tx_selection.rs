//! Benchmarks for transaction selection in the flashblocks payload build loop.
//!
//! Most benchmarks exclude transaction construction and EVM execution. They cover reth's plain
//! pending-pool `pool.best()` iterator, the lane-aware parking adapter used by flashblocks, and the
//! validity-predicate parking and state-index wakeup cycle. The resting-predicate benchmark runs
//! whole blocks through the build loop, including execution and transaction events.

use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use base_builder_core::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, BlockRejections, ExecutionInfo,
    ParkableBestPayloadTransactions, ParkablePayloadTransactions, ParkedPredicateIndex,
    RejectionCache, ResourceLimits, RestingPredicateMode, ValidityPredicateKey,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, PredicateContext,
    ValidityOperator, ValidityPredicate,
};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reth_chainspec::ChainSpec;
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::{Recovered, SealedHeader};
use reth_revm::State;
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, TransactionOrigin, ValidPoolTransaction,
    identifier::TransactionId, pool::PendingPool,
};
use revm::{
    database::InMemoryDB,
    state::{Account, AccountInfo, EvmState},
};

type Ordering = BaseOrdering<BasePooledTransaction>;
type Pool = PendingPool<Ordering>;

const TRANSACTION_COUNTS: &[usize] = &[1_000, 10_000, 100_000];
const PREDICATE_TRANSACTION_COUNT: usize = 10_000;

fn address(index: usize) -> Address {
    Address::from_word(U256::from(index + 1).into())
}

fn predicates(
    transaction_index: usize,
    count: usize,
    unique_state: bool,
) -> Vec<ValidityPredicate> {
    (0..count)
        .map(|predicate_index| ValidityPredicate::Balance {
            address: address(if unique_state {
                TRANSACTION_COUNTS[TRANSACTION_COUNTS.len() - 1]
                    + transaction_index * count
                    + predicate_index
            } else {
                TRANSACTION_COUNTS[TRANSACTION_COUNTS.len() - 1] + predicate_index
            }),
            op: ValidityOperator::Equal,
            value: if predicate_index + 1 == count { U256::ONE } else { U256::ZERO },
        })
        .collect()
}

fn transaction(
    index: usize,
    sender_index: usize,
    nonce: u64,
    predicate_count: usize,
    unique_predicate_state: bool,
) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let tx = TxEip1559 {
        chain_id: 1,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 1,
        to: TxKind::Call(Address::ZERO),
        value: U256::from(index),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
    let encoded_length = envelope.encode_2718_len();
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), address(sender_index)),
        encoded_length,
    )
    .with_validity_predicates(predicates(index, predicate_count, unique_predicate_state));

    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new((sender_index as u64).into(), nonce),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

fn pool(
    transaction_count: usize,
    sender_count: usize,
    predicate_transactions: usize,
    predicates_per_transaction: usize,
    unique_predicate_state: bool,
) -> Pool {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    for index in 0..transaction_count {
        let sender_index = index % sender_count;
        let predicate_count =
            usize::from(index < predicate_transactions) * predicates_per_transaction;
        pool.add_transaction(
            transaction(
                index,
                sender_index,
                (index / sender_count) as u64,
                predicate_count,
                unique_predicate_state,
            ),
            0,
        );
    }
    pool
}

fn parkable(pool: &Pool) -> ParkableBestPayloadTransactions<BasePooledTransaction> {
    let mut best = pool.best();
    best.no_updates();
    ParkableBestPayloadTransactions::new(Box::new(ParkedBestTransactions::new(
        best,
        Ordering::coinbase_tip(),
        0,
    )))
}

fn selection_benches(c: &mut Criterion) {
    let mut plain = c.benchmark_group("tx_selection/best_transactions");
    plain.sample_size(10);
    for &transaction_count in TRANSACTION_COUNTS {
        let pool = pool(transaction_count, transaction_count, 0, 0, false);
        plain.throughput(Throughput::Elements(transaction_count as u64));
        plain.bench_with_input(
            BenchmarkId::new("end_to_end", transaction_count),
            &transaction_count,
            |b, _| {
                b.iter(|| {
                    let mut best = pool.best();
                    best.no_updates();
                    let mut selected = 0;
                    for transaction in best {
                        black_box(transaction);
                        selected += 1;
                    }
                    assert_eq!(selected, transaction_count);
                });
            },
        );
        plain.bench_with_input(
            BenchmarkId::new("snapshot", transaction_count),
            &transaction_count,
            |b, _| {
                b.iter(|| {
                    let mut best = pool.best();
                    best.no_updates();
                    black_box(best);
                });
            },
        );
        plain.bench_with_input(
            BenchmarkId::new("iterate", transaction_count),
            &transaction_count,
            |b, _| {
                b.iter_batched(
                    || {
                        let mut best = pool.best();
                        best.no_updates();
                        best
                    },
                    |best| {
                        let mut selected = 0;
                        for transaction in best {
                            black_box(transaction);
                            selected += 1;
                        }
                        assert_eq!(selected, transaction_count);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    plain.finish();

    let mut chained = c.benchmark_group("tx_selection/best_transactions_chained");
    chained.sample_size(10);
    chained.throughput(Throughput::Elements(100_000));
    for sender_count in [1, 1_000] {
        let pool = pool(100_000, sender_count, 0, 0, false);
        chained.bench_function(format!("{sender_count}_senders/100000"), |b| {
            b.iter(|| {
                let mut best = pool.best();
                best.no_updates();
                assert_eq!(best.by_ref().map(black_box).count(), 100_000);
            });
        });
    }
    chained.finish();

    let mut parking = c.benchmark_group("tx_selection/parkable_payload");
    parking.sample_size(10);
    for &transaction_count in TRANSACTION_COUNTS {
        let pool = pool(transaction_count, transaction_count, 0, 0, false);
        parking.throughput(Throughput::Elements(transaction_count as u64));
        parking.bench_with_input(
            BenchmarkId::new("transactions", transaction_count),
            &transaction_count,
            |b, _| {
                b.iter(|| {
                    let mut best = parkable(&pool);
                    let mut selected = 0;
                    while let Some(transaction) = best.next(()) {
                        black_box(transaction);
                        best.mark_current_committed();
                        selected += 1;
                    }
                    assert_eq!(selected, transaction_count);
                });
            },
        );
    }
    parking.finish();
}

fn run_predicate_selection(pool: &Pool, db: &mut InMemoryDB) -> usize {
    let mut best = parkable(pool);
    let mut predicate_index = ParkedPredicateIndex::default();
    let mut selected = 0;
    // These benchmarks exercise state predicates, so the build position is irrelevant.
    let context = PredicateContext { block_number: 0, flashblock_index: 0 };

    while let Some(transaction) = best.next(()) {
        let blocking_predicate = ValidityPredicateKey::first_unsatisfied(
            transaction.validity_predicates(),
            db,
            &context,
        )
        .expect("in-memory reads cannot fail");
        if let Some((blocking_predicate_index, _)) = blocking_predicate {
            let transaction_hash = *transaction.hash();
            let predicate = transaction.validity_predicates()[blocking_predicate_index].clone();
            best.park_current();
            predicate_index.park(transaction_hash, transaction, predicate);
            continue;
        }

        black_box(&transaction);
        best.mark_current_committed();
        selected += 1;
        assert!(
            predicate_index
                .affected_by_state(&EvmState::default())
                .affected_transactions
                .is_empty()
        );
    }

    selected
}

fn predicate_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_rescan");
    group.sample_size(10);
    group.throughput(Throughput::Elements(PREDICATE_TRANSACTION_COUNT as u64));

    for &(predicate_transactions, predicates_per_transaction, unique_state) in &[
        (10, 1, false),
        (100, 1, false),
        (1_000, 1, false),
        (10, 8, false),
        (100, 8, false),
        (1_000, 8, false),
        (100, 8, true),
    ] {
        let pool = pool(
            PREDICATE_TRANSACTION_COUNT,
            PREDICATE_TRANSACTION_COUNT,
            predicate_transactions,
            predicates_per_transaction,
            unique_state,
        );
        let name = format!(
            "transactions={PREDICATE_TRANSACTION_COUNT}/predicate_transactions={predicate_transactions}/predicates={predicates_per_transaction}/state={}",
            if unique_state { "unique" } else { "shared" }
        );
        group.bench_function(name, |b| {
            b.iter_batched(
                InMemoryDB::default,
                |mut db| {
                    let selected = run_predicate_selection(&pool, &mut db);
                    assert_eq!(selected, PREDICATE_TRANSACTION_COUNT - predicate_transactions);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn predicate_index_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_index");
    group.sample_size(10);

    for parked_transactions in [1_000, 10_000, 100_000] {
        for shared_state in [false, true] {
            let shared_address = address(TRANSACTION_COUNTS[TRANSACTION_COUNTS.len() - 1]);
            let mut index = ParkedPredicateIndex::new(if shared_state { 32 } else { usize::MAX });
            for transaction_index in 0..parked_transactions {
                let transaction_hash: B256 = U256::from(transaction_index + 1).into();
                let predicate_address =
                    if shared_state { shared_address } else { address(transaction_index) };
                index.park(
                    transaction_hash,
                    (),
                    ValidityPredicate::Balance {
                        address: predicate_address,
                        op: ValidityOperator::GreaterThanOrEqual,
                        value: U256::MAX,
                    },
                );
            }

            let changed_address = if shared_state { shared_address } else { address(0) };
            let mut changed_state = EvmState::default();
            let mut changed_account = Account::default();
            changed_account.info.balance = U256::ONE;
            changed_state.insert(changed_address, changed_account);

            group.bench_function(
                format!(
                    "parked={parked_transactions}/state={}",
                    if shared_state { "shared" } else { "unique" }
                ),
                |b| b.iter(|| black_box(index.affected_by_state(&changed_state))),
            );
        }
    }
    group.finish();
}

const RESTING_CHAIN_ID: u64 = 901;
const RESTING_FLASHBLOCKS: usize = 10;
const RESTING_FILLER_PER_FLASHBLOCK: usize = 100;
const TRANSFER_GAS: u64 = 21_000;
/// Sender indexes for the resting-predicate block start here so they do not collide with
/// the watched predicate addresses.
const RESTING_SENDER_OFFSET: usize = 1_000_000;

/// Builds an executable transfer. Resting transactions get a higher tip so every flashblock
/// considers them before the filler that fills it.
fn executable_transaction(
    sender_index: usize,
    priority_fee: u128,
    predicates: Vec<ValidityPredicate>,
) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
    let tx = TxEip1559 {
        chain_id: RESTING_CHAIN_ID,
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
    let sender = address(RESTING_SENDER_OFFSET + sender_index);
    let transaction = BasePooledTransaction::new(
        Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender),
        encoded_length,
    )
    .with_validity_predicates(predicates);

    Arc::new(ValidPoolTransaction {
        transaction_id: TransactionId::new(
            ((RESTING_SENDER_OFFSET + sender_index) as u64).into(),
            0,
        ),
        transaction,
        propagate: true,
        timestamp: Instant::now(),
        origin: TransactionOrigin::External,
        authority_ids: None,
    })
}

/// A pool holding `resting` validity transactions whose balance predicates never become
/// satisfied, plus enough plain transfers to fill every flashblock.
fn resting_block(resting: usize) -> (Pool, InMemoryDB) {
    let filler = RESTING_FLASHBLOCKS * RESTING_FILLER_PER_FLASHBLOCK;
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut db = InMemoryDB::default();
    for sender_index in 0..resting + filler {
        let predicates = if sender_index < resting {
            vec![ValidityPredicate::Balance {
                address: address(sender_index),
                op: ValidityOperator::Equal,
                value: U256::ONE,
            }]
        } else {
            Vec::new()
        };
        let priority_fee = if sender_index < resting { 10 } else { 1 };
        pool.add_transaction(executable_transaction(sender_index, priority_fee, predicates), 0);
        db.insert_account_info(
            address(RESTING_SENDER_OFFSET + sender_index),
            AccountInfo { balance: U256::from(u128::MAX), ..Default::default() },
        );
    }
    (pool, db)
}

fn resting_builder_context() -> BasePayloadBuilderCtx {
    let genesis = serde_json::from_value(serde_json::json!({
        "config": { "chainId": RESTING_CHAIN_ID },
        "gasLimit": "0x1C9C380",
        "timestamp": "0x0"
    }))
    .expect("valid genesis");
    let chain_spec = ChainSpec::builder()
        .chain(RESTING_CHAIN_ID.into())
        .genesis(genesis)
        .cancun_activated()
        .build();
    let parent = Header { gas_limit: 30_000_000, timestamp: 0, ..Default::default() };
    BasePayloadBuilderCtx::for_test(
        Arc::new(BaseChainSpec::from(chain_spec)),
        Arc::new(SealedHeader::seal_slow(parent)),
    )
}

/// Runs every flashblock of one block through the build loop and returns the number of
/// executed transactions.
fn run_resting_block(
    ctx: &BasePayloadBuilderCtx,
    pool: &Pool,
    state: &mut State<InMemoryDB>,
    mode: RestingPredicateMode,
) -> usize {
    let mut info = ExecutionInfo::default();
    let mut deferrals = BlockDeferrals::default();
    let mut rejections = BlockRejections::default();
    let mut best = BestFlashblocksTxs::new(
        parkable(pool),
        RejectionCache::new(1_000, Duration::from_secs(60)),
    )
    .with_resting_predicate_mode(mode);
    let mut committed_until = 0;

    for flashblock in 0..RESTING_FLASHBLOCKS {
        if flashblock > 0 {
            best.refresh_iterator(parkable(pool));
        }
        let limits = ResourceLimits {
            block_gas_limit: ((flashblock + 1) * RESTING_FILLER_PER_FLASHBLOCK) as u64
                * TRANSFER_GAS,
            ..Default::default()
        };
        black_box(
            ctx.execute_best_transactions(
                &mut info,
                &mut deferrals,
                &mut rejections,
                state,
                &mut best,
                &limits,
            )
            .expect("in-memory execution cannot fail"),
        );
        let committed = info.executed_transactions[committed_until..]
            .iter()
            .map(|transaction| transaction.tx_hash())
            .collect::<Vec<_>>();
        best.mark_committed(&committed);
        committed_until = info.executed_transactions.len();
    }

    info.executed_transactions.len()
}

/// Measures a full block in which `resting` validity transactions stay unsatisfied. With
/// resting predicates off, every flashblock re-evaluates them and emits a deferral event;
/// enforced, the iterator holds them back after the first evaluation.
fn resting_predicate_benches(c: &mut Criterion) {
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

    let ctx = resting_builder_context();
    let mut group = c.benchmark_group("tx_selection/resting_predicates");
    group.sample_size(10);
    for resting in [100, 1_000, 10_000] {
        let (pool, db) = resting_block(resting);
        for mode in [RestingPredicateMode::Off, RestingPredicateMode::Enforce] {
            group.bench_function(
                format!("resting={resting}/flashblocks={RESTING_FLASHBLOCKS}/mode={mode:?}"),
                |b| {
                    b.iter_batched(
                        || State::builder().with_database(db.clone()).with_bundle_update().build(),
                        |mut state| {
                            let executed = run_resting_block(&ctx, &pool, &mut state, mode);
                            assert_eq!(
                                executed,
                                RESTING_FLASHBLOCKS * RESTING_FILLER_PER_FLASHBLOCK
                            );
                        },
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    selection_benches,
    predicate_benches,
    predicate_index_benches,
    predicate_index_threshold_benches,
    predicate_index_lifecycle_benches,
    predicate_index_population_benches,
    predicate_index_crossing_benches,
    resting_predicate_benches
);
criterion_main!(benches);

// Kept separate so `cargo bench -p base-builder-core --bench tx_selection --
// predicate_index_threshold --sample-size 10` completes quickly.
fn predicate_index_threshold_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_index_threshold");
    group.sample_size(10);
    let watched_address = address(999_999);

    for (name, threshold, parked_transactions) in [
        ("flat/shallow", usize::MAX, 16),
        ("hybrid/shallow", 32, 16),
        ("ordered/shallow", 1, 16),
        ("flat/hot_deep", usize::MAX, 2_048),
        ("hybrid/hot_deep", 32, 2_048),
        ("ordered/hot_deep", 1, 2_048),
    ] {
        let mut index = ParkedPredicateIndex::new(threshold);
        for transaction_index in 0..parked_transactions {
            let transaction_hash: B256 = U256::from(transaction_index + 1).into();
            index.park(
                transaction_hash,
                (),
                ValidityPredicate::Balance {
                    address: watched_address,
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::MAX,
                },
            );
        }
        let mut changed_account = Account::default();
        changed_account.info.balance = U256::ONE;
        let changed_state = EvmState::from_iter([(watched_address, changed_account)]);
        group.bench_function(name, |b| {
            b.iter(|| {
                let effects = index.affected_by_state(&changed_state);
                let expected = usize::from(parked_transactions < threshold) * parked_transactions;
                assert_eq!(effects.affected_transactions.len(), expected);
                black_box(effects);
            });
        });
    }
    group.finish();
}

// Kept separate so `cargo bench -p base-builder-core --bench tx_selection --
// predicate_index_population --sample-size 10` completes quickly.
fn predicate_index_population_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_index_population");
    group.sample_size(10);

    for parked_transactions in [100, 500, 1_000] {
        group.bench_function(format!("distinct_shallow/entries={parked_transactions}"), |b| {
            b.iter(|| {
                let mut index = ParkedPredicateIndex::new(32);
                for transaction_index in 0..parked_transactions {
                    let transaction_hash: B256 = U256::from(transaction_index + 1).into();
                    index.park(
                        transaction_hash,
                        (),
                        ValidityPredicate::Balance {
                            address: address(transaction_index),
                            op: ValidityOperator::GreaterThanOrEqual,
                            value: U256::MAX,
                        },
                    );
                }
                assert!(!index.is_empty());
                black_box(index);
            });
        });
    }
    group.finish();
}

// Kept separate so `cargo bench -p base-builder-core --bench tx_selection --
// predicate_index_crossing --sample-size 10` completes quickly.
fn predicate_index_crossing_benches(c: &mut Criterion) {
    const PARKED_TRANSACTIONS: usize = 2_048;

    let mut group = c.benchmark_group("tx_selection/predicate_index_crossing");
    group.sample_size(10);
    let watched_address = address(777_777);
    let mut changed_account = Account::default();
    changed_account.info.balance = U256::from(PARKED_TRANSACTIONS);
    let changed_state = EvmState::from_iter([(watched_address, changed_account)]);

    for (name, threshold) in [
        ("flat/all_thresholds", usize::MAX),
        ("hybrid/all_thresholds", 32),
        ("ordered/all_thresholds", 1),
    ] {
        let mut index = ParkedPredicateIndex::new(threshold);
        for transaction_index in 0..PARKED_TRANSACTIONS {
            let transaction_hash: B256 = U256::from(transaction_index + 1).into();
            index.park(
                transaction_hash,
                (),
                ValidityPredicate::Balance {
                    address: watched_address,
                    op: ValidityOperator::GreaterThanOrEqual,
                    value: U256::from(transaction_index + 1),
                },
            );
        }
        group.bench_function(name, |b| {
            b.iter(|| {
                let effects = index.affected_by_state(&changed_state);
                assert_eq!(effects.affected_transactions.len(), PARKED_TRANSACTIONS);
                black_box(effects);
            });
        });
    }
    group.finish();
}

// Includes bucket construction and parking so tiny-bucket conversion overhead is visible.
fn predicate_index_lifecycle_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_index_lifecycle");
    group.sample_size(10);
    let watched_address = address(888_888);
    let mut changed_account = Account::default();
    changed_account.info.balance = U256::ONE;
    let changed_state = EvmState::from_iter([(watched_address, changed_account)]);

    for (name, threshold, parked_transactions) in [
        ("flat/one", usize::MAX, 1),
        ("ordered/one", 1, 1),
        ("flat/four", usize::MAX, 4),
        ("ordered/four", 1, 4),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut index = ParkedPredicateIndex::new(threshold);
                for transaction_index in 0..parked_transactions {
                    let transaction_hash: B256 = U256::from(transaction_index + 1).into();
                    index.park(
                        transaction_hash,
                        (),
                        ValidityPredicate::Balance {
                            address: watched_address,
                            op: ValidityOperator::GreaterThanOrEqual,
                            value: U256::MAX,
                        },
                    );
                }
                let effects = index.affected_by_state(&changed_state);
                let expected = usize::from(parked_transactions < threshold) * parked_transactions;
                assert_eq!(effects.affected_transactions.len(), expected);
                black_box(effects);
            });
        });
    }
    group.finish();
}
