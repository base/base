//! Benchmarks for transaction selection in the flashblocks payload build loop.
//!
//! Transaction construction and EVM execution are excluded. The benchmarks cover the production
//! non-parking pending-pool iterator, the lane-aware parking adapter used by flashblocks, and the
//! validity-predicate park/commit/rescan cycle.

use std::{hint::black_box, sync::Arc, time::Instant};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use base_builder_core::{ParkableBestPayloadTransactions, ParkablePayloadTransactions};
use base_common_consensus::{BaseTransactionSigned, BaseTxEnvelope};
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, ParkedBestTransactions, PredicateContext,
    ValidityOperator, ValidityPredicate,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reth_payload_util::PayloadTransactions;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    BestTransactions, TransactionOrigin, ValidPoolTransaction, identifier::TransactionId,
    pool::PendingPool,
};
use revm::database::InMemoryDB;

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
    let mut selected = 0;
    // These benchmarks exercise state predicates, so the build position is irrelevant.
    let context = PredicateContext { block_number: 0, flashblock_index: 0 };

    while let Some(transaction) = best.next(()) {
        let predicates_match = transaction
            .validity_predicates()
            .iter()
            .all(|predicate| predicate.matches(db, &context).expect("in-memory reads cannot fail"));
        if !predicates_match {
            assert!(best.park_current());
            continue;
        }

        black_box(&transaction);
        best.mark_current_committed();
        selected += 1;

        for parked in best.parked_transactions() {
            let matches = parked.transaction.validity_predicates().iter().all(|predicate| {
                predicate.matches(db, &context).expect("in-memory reads cannot fail")
            });
            if matches {
                assert!(best.promote(*parked.hash()));
            }
        }
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

criterion_group!(benches, selection_benches, predicate_benches);
criterion_main!(benches);
