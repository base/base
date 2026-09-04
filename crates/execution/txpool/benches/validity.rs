//! Benchmarks for validity-predicate ingress and expiry checks.
//!
//! Every transaction submitted via `base_sendRawTransactionValidity` carries a
//! batch of up to [`DEFAULT_MAX_VALIDITY_PREDICATES`] predicates that is walked
//! on the ingress path ([`ValidityPredicate::validate_batch`]) and, for the
//! block-number–bounded ones, projected to a pool-side expiry bound on every
//! admission ([`ValidityPredicate::block_expiry_bound`]) and re-checked at build
//! positions ([`ValidityPredicate::is_batch_expired`]). All three are pure,
//! allocation-free, `O(predicates)` scans with no state reads, so they are a
//! good per-PR advisory signal; the `validity_iai` twin captures the
//! deterministic instruction count.

use std::hint::black_box;

use alloy_primitives::{Address, U256};
use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_PREDICATES, PredicateContext, ValidityOperator, ValidityPredicate,
};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// The batch sizes swept by every group: a single predicate, a typical handful,
/// and a full [`DEFAULT_MAX_VALIDITY_PREDICATES`] batch (the adversarial worst
/// case the ingress path must tolerate).
const BATCH_SIZES: [usize; 3] = [1, 8, DEFAULT_MAX_VALIDITY_PREDICATES];

/// Builds a batch of `count` internally valid predicates cycling through all
/// four variants, so `validate_batch` walks and validates every element instead
/// of short-circuiting. Each variant is shaped to pass `validate_params`.
fn mixed_batch(count: usize) -> Vec<ValidityPredicate> {
    (0..count)
        .map(|i| match i % 4 {
            0 => ValidityPredicate::Balance {
                address: Address::repeat_byte(i as u8),
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(i as u64 + 1),
            },
            1 => ValidityPredicate::Storage {
                address: Address::repeat_byte(i as u8),
                slot: U256::from(i as u64),
                mask: U256::from(0xffu64),
                op: ValidityOperator::Equal,
                value: U256::from((i as u64) & 0xff),
            },
            2 => ValidityPredicate::BlockNumber {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(i as u64),
            },
            _ => ValidityPredicate::FlashblockIndex {
                op: ValidityOperator::GreaterThanOrEqual,
                value: U256::from(i as u64 + 1),
            },
        })
        .collect()
}

/// Builds a batch of `count` upper-bounded positional predicates (alternating
/// block-number and flashblock-index) with distinct bounds, forcing
/// `block_expiry_bound` / `is_batch_expired` to fold the tightest bound across
/// every element rather than stop at the first.
fn positional_batch(count: usize) -> Vec<ValidityPredicate> {
    (0..count)
        .map(|i| {
            let value = U256::from(1_000 - i as u64);
            if i % 2 == 0 {
                ValidityPredicate::BlockNumber { op: ValidityOperator::LessThanOrEqual, value }
            } else {
                ValidityPredicate::FlashblockIndex { op: ValidityOperator::LessThanOrEqual, value }
            }
        })
        .collect()
}

fn bench_validity(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity/validate_batch");
    for count in BATCH_SIZES {
        let batch = mixed_batch(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &batch, |b, batch| {
            b.iter(|| {
                black_box(ValidityPredicate::validate_batch(
                    black_box(batch),
                    DEFAULT_MAX_VALIDITY_PREDICATES,
                ))
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("validity/block_expiry_bound");
    for count in BATCH_SIZES {
        let batch = positional_batch(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &batch, |b, batch| {
            b.iter(|| black_box(ValidityPredicate::block_expiry_bound(black_box(batch))));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("validity/is_batch_expired");
    // A build position that leaves every bound in `positional_batch` satisfiable,
    // so the check performs the full fold instead of short-circuiting.
    let context = PredicateContext { block_number: 10, flashblock_index: 1 };
    for count in BATCH_SIZES {
        let batch = positional_batch(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &batch, |b, batch| {
            b.iter(|| {
                black_box(ValidityPredicate::is_batch_expired(
                    black_box(batch),
                    black_box(&context),
                ))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_validity);
criterion_main!(benches);
