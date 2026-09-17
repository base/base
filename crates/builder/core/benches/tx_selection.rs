//! Benchmarks for transaction selection in the flashblocks payload build loop.
//!
//! Transaction construction and EVM execution are excluded. The benchmarks cover the production
//! non-parking pending-pool iterator, the lane-aware parking adapter used by flashblocks, and the
//! validity-predicate parking and state-index wakeup cycle.

use std::{hint::black_box, sync::Arc, time::Instant};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use base_builder_core::{
    ParkableBestPayloadTransactions, ParkablePayloadTransactions, ParkedPredicateIndex,
    ValidityPredicateKey,
};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, PredicateContext,
    ValidityOperator, ValidityPredicate,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, TransactionOrigin, ValidPoolTransaction,
    identifier::TransactionId, pool::PendingPool,
};
use revm::{
    database::InMemoryDB,
    state::{Account, EvmState},
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
            assert!(best.park_current());
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

criterion_group!(
    benches,
    selection_benches,
    predicate_benches,
    predicate_index_benches,
    predicate_index_threshold_benches,
    predicate_index_lifecycle_benches,
    predicate_index_population_benches,
    predicate_index_crossing_benches
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
