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
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, ValidityOperator,
    ValidityPredicate,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::Recovered;
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
    validity_predicates: Vec<ValidityPredicate>,
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
    .with_validity_predicates(validity_predicates);

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
                predicates(index, predicate_count, unique_predicate_state),
            ),
            0,
        );
    }
    pool
}

fn skewed_predicates(transaction_index: usize, count: usize) -> Vec<ValidityPredicate> {
    (0..count)
        .map(|predicate_index| ValidityPredicate::Balance {
            address: if predicate_index == 0 {
                Address::ZERO
            } else {
                address(
                    TRANSACTION_COUNTS[TRANSACTION_COUNTS.len() - 1]
                        + transaction_index * count
                        + predicate_index,
                )
            },
            op: ValidityOperator::Equal,
            value: U256::ONE,
        })
        .collect()
}

fn skewed_pool(
    transaction_count: usize,
    predicates_per_transaction: usize,
    shuffle_predicates: bool,
) -> Pool {
    let mut pool = PendingPool::new(Ordering::coinbase_tip());
    let mut rng = StdRng::seed_from_u64(0x5eed);
    for index in 0..transaction_count {
        let mut predicates = skewed_predicates(index, predicates_per_transaction);
        if shuffle_predicates {
            predicates.shuffle(&mut rng);
        }
        pool.add_transaction(transaction(index, index, 0, predicates), 0);
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

fn run_predicate_selection(
    pool: &Pool,
    db: &mut InMemoryDB,
    hash_rotated: bool,
) -> (usize, ParkedPredicateIndex<BasePooledTransaction>) {
    let mut best = parkable(pool);
    let mut predicate_index = ParkedPredicateIndex::default();
    let mut selected = 0;

    while let Some(transaction) = best.next(()) {
        let predicates = transaction.validity_predicates();
        let blocking_predicate = ValidityPredicateKey::find_map_in_scan_order(
            predicates,
            *transaction.hash(),
            hash_rotated,
            |predicate| {
                (!predicate.matches_state(db).expect("in-memory reads cannot fail"))
                    .then_some(ValidityPredicateKey::for_predicate(predicate))
            },
        );
        if let Some(blocking_predicate) = blocking_predicate {
            let transaction_hash = *transaction.hash();
            assert!(best.park_current());
            predicate_index.park(transaction_hash, transaction, blocking_predicate);
            continue;
        }

        black_box(&transaction);
        best.mark_current_committed();
        selected += 1;
        assert!(predicate_index.affected_by_state(&EvmState::default()).is_empty());
    }

    (selected, predicate_index)
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
                    let (selected, _) = run_predicate_selection(&pool, &mut db, true);
                    assert_eq!(selected, PREDICATE_TRANSACTION_COUNT - predicate_transactions);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn predicate_skew_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_skew");
    group.sample_size(10);

    for (transaction_count, predicates_per_transaction, order) in [
        (10_000, 2, "correlated"),
        (10_000, 8, "correlated"),
        (10_000, 64, "correlated"),
        (100_000, 2, "correlated"),
        (100_000, 8, "correlated"),
        (100_000, 2, "shuffled"),
        (100_000, 8, "shuffled"),
    ] {
        group.throughput(Throughput::Elements(transaction_count as u64));
        let pool = skewed_pool(transaction_count, predicates_per_transaction, order == "shuffled");
        for (policy, hash_rotated) in [("first", false), ("hash_rotated", true)] {
            group.bench_function(
                format!(
                    "drain/order={order}/policy={policy}/n={transaction_count}/p={predicates_per_transaction}"
                ),
                |b| {
                    b.iter_batched(
                        InMemoryDB::default,
                        |mut db| {
                            assert_eq!(
                                run_predicate_selection(&pool, &mut db, hash_rotated).0,
                                0
                            );
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
    }
    group.finish();
}

fn run_hot_key_cycle(pool: &Pool, hash_rotated: bool) -> usize {
    let mut db = InMemoryDB::default();
    let (selected, mut predicate_index) = run_predicate_selection(pool, &mut db, hash_rotated);
    assert_eq!(selected, 0);

    db.insert_account_info(Address::ZERO, AccountInfo { balance: U256::ONE, ..Default::default() });
    let mut changed_state = EvmState::default();
    let mut changed_account = Account::default();
    changed_account.info.balance = U256::ONE;
    changed_state.insert(Address::ZERO, changed_account);

    let affected = predicate_index.affected_by_state(&changed_state);
    for transaction_hash in &affected {
        let predicates = predicate_index
            .transaction(*transaction_hash)
            .expect("affected transaction must remain indexed")
            .validity_predicates();
        let blocker = ValidityPredicateKey::find_map_in_scan_order(
            predicates,
            *transaction_hash,
            hash_rotated,
            |predicate| {
                (!predicate.matches_state(&mut db).expect("in-memory reads cannot fail"))
                    .then_some(ValidityPredicateKey::for_predicate(predicate))
            },
        )
        .expect("each transaction retains an unsatisfied unique predicate");
        assert!(predicate_index.reindex(*transaction_hash, blocker));
    }

    black_box(&predicate_index);
    affected.len()
}

fn predicate_hot_key_cycle_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_hot_key_cycle");
    group.sample_size(10);
    group.throughput(Throughput::Elements(100_000));

    for predicates_per_transaction in [2, 8] {
        let pool = skewed_pool(100_000, predicates_per_transaction, false);
        for (policy, hash_rotated) in [("first", false), ("hash_rotated", true)] {
            group.bench_function(
                format!("policy={policy}/n=100000/p={predicates_per_transaction}"),
                |b| {
                    b.iter(|| {
                        let affected = run_hot_key_cycle(&pool, hash_rotated);
                        if hash_rotated {
                            assert!(affected < 100_000);
                        } else {
                            assert_eq!(affected, 100_000);
                        }
                    });
                },
            );
        }
    }
    group.finish();
}

fn skewed_predicate_index(
    parked_transactions: usize,
    predicates_per_transaction: usize,
    hash_rotated: bool,
) -> ParkedPredicateIndex<()> {
    let mut index = ParkedPredicateIndex::default();
    for transaction_index in 0..parked_transactions {
        let transaction_hash: B256 = U256::from(transaction_index).into();
        let predicate_index =
            if hash_rotated { transaction_index % predicates_per_transaction } else { 0 };
        let blocker = if predicate_index == 0 {
            ValidityPredicateKey::Balance(Address::ZERO)
        } else {
            ValidityPredicateKey::Balance(address(
                TRANSACTION_COUNTS[TRANSACTION_COUNTS.len() - 1]
                    + transaction_index * predicates_per_transaction
                    + predicate_index,
            ))
        };
        index.park(transaction_hash, (), blocker);
    }
    index
}

fn predicate_index_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_selection/predicate_index");
    group.sample_size(10);

    for parked_transactions in [1_000, 10_000, 100_000] {
        for shared_state in [false, true] {
            let shared_address = address(TRANSACTION_COUNTS[TRANSACTION_COUNTS.len() - 1]);
            let mut index = ParkedPredicateIndex::default();
            for transaction_index in 0..parked_transactions {
                let transaction_hash: B256 = U256::from(transaction_index + 1).into();
                let predicate_address =
                    if shared_state { shared_address } else { address(transaction_index) };
                index.park(transaction_hash, (), ValidityPredicateKey::Balance(predicate_address));
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

    let mut hot_state = EvmState::default();
    let mut hot_account = Account::default();
    hot_account.info.balance = U256::ONE;
    hot_state.insert(Address::ZERO, hot_account);
    for parked_transactions in [10_000, 100_000] {
        for predicates_per_transaction in [2, 8, 64] {
            for (policy, hash_rotated) in [("first", false), ("hash_rotated", true)] {
                let index = skewed_predicate_index(
                    parked_transactions,
                    predicates_per_transaction,
                    hash_rotated,
                );
                let expected_affected = if hash_rotated {
                    parked_transactions.div_ceil(predicates_per_transaction)
                } else {
                    parked_transactions
                };
                assert_eq!(index.affected_by_state(&hot_state).len(), expected_affected);
                group.bench_function(
                    format!(
                        "skew/parked={parked_transactions}/predicates={predicates_per_transaction}/policy={policy}"
                    ),
                    |b| b.iter(|| black_box(index.affected_by_state(&hot_state))),
                );
            }
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    selection_benches,
    predicate_benches,
    predicate_skew_benches,
    predicate_hot_key_cycle_benches,
    predicate_index_benches
);
criterion_main!(benches);
