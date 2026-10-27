//! Per-admission and per-block cost of the validity pool budget (`ValidityBudget`), the
//! count-and-bytes cap that keeps resting validity transactions from crowding standard
//! transactions out of the protocol pool.
//!
//! Each arm is one operation the pool pays, at the occupancy it runs at:
//!
//! - `admission/evicting`: `plan_admission` + `commit` into a full budget where every
//!   bid outbids the floor, so each admission evicts the cheapest resident.
//! - `admission/rejected`: `plan_admission` into a full budget where every bid loses,
//!   the steady state under a fixed-tip validity flood.
//! - `admission/vacant`: `plan_admission` + `commit` into a budget with headroom, the
//!   steady state today.
//! - `release`: `remove` of a counted hash, paid once per inclusion, replacement, or expiry.
//! - `reconcile`: `tracked_hashes` plus a membership check and `remove` for the stale
//!   entries, paid once per canonical block.
//!
//! Occupancies cover the default budget (10k) and the proposed mainnet sequencer budget (50k).
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench validity_budget
//! ```

use std::{collections::HashSet, hint::black_box};

use alloy_primitives::{B256, TxHash};
use base_execution_txpool::ValidityBudget;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reth_transaction_pool::SubPoolLimit;

/// Encoded size of a typical validity transaction with a handful of predicates.
const TX_SIZE: usize = 400;
const OCCUPANCIES: [usize; 2] = [10_000, 50_000];

/// Deterministic hash sequence; hashes only need to be distinct.
fn hash(index: u64) -> TxHash {
    B256::from(alloy_primitives::U256::from(index).to_be_bytes::<32>())
}

/// A pseudo-random bid in `[0, max)`, so eviction order is not insertion order.
fn priority(index: u64, max: u128) -> u128 {
    u128::from(index.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17)) % max
}

fn filled(residents: usize, limit: SubPoolLimit) -> ValidityBudget {
    let mut budget = ValidityBudget::new(limit);
    for index in 0..residents as u64 {
        budget.insert(hash(index), priority(index, 1_000), TX_SIZE);
    }
    budget
}

fn admission(c: &mut Criterion) {
    for occupancy in OCCUPANCIES {
        let mut group = c.benchmark_group("validity_budget/admission");
        group.throughput(Throughput::Elements(1));

        // Evicting: `occupancy` residents against a limit of `occupancy`, and every bid
        // outbids the floor, so each admission evicts the cheapest resident. The
        // steady state under a validity flood that keeps raising its tip.
        let mut budget = filled(occupancy, SubPoolLimit::new(occupancy, usize::MAX));
        let mut next = occupancy as u64;
        group.bench_with_input(BenchmarkId::new("evicting", occupancy), &occupancy, |b, _| {
            b.iter(|| {
                next += 1;
                let bid = 1_000 + u128::from(next);
                let admission = budget.plan_admission(hash(next), bid, TX_SIZE, None).unwrap();
                black_box(budget.commit(admission))
            })
        });

        // Rejected: the same full budget, and every bid loses to the floor. The steady
        // state under a validity flood at a fixed tip once the budget is full.
        let budget = filled(occupancy, SubPoolLimit::new(occupancy, usize::MAX));
        let mut next = occupancy as u64;
        group.bench_with_input(BenchmarkId::new("rejected", occupancy), &occupancy, |b, _| {
            b.iter(|| {
                next += 1;
                black_box(budget.plan_admission(hash(next), 0, TX_SIZE, None).unwrap_err())
            })
        });

        // Vacant: the same residents against a limit twice as large, so admissions
        // never evict. Remove the newcomer again so occupancy stays put.
        let mut budget = filled(occupancy, SubPoolLimit::new(occupancy * 2, usize::MAX));
        let mut next = occupancy as u64;
        group.bench_with_input(BenchmarkId::new("vacant", occupancy), &occupancy, |b, _| {
            b.iter(|| {
                next += 1;
                let newcomer = hash(next);
                let admission =
                    budget.plan_admission(newcomer, priority(next, 1_000), TX_SIZE, None).unwrap();
                black_box(budget.commit(admission));
                budget.remove(&newcomer)
            })
        });
        group.finish();
    }
}

fn release(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_budget/release");
    group.throughput(Throughput::Elements(1));
    for occupancy in OCCUPANCIES {
        let mut budget = filled(occupancy, SubPoolLimit::new(occupancy, usize::MAX));
        let mut cursor = 0u64;
        group.bench_with_input(BenchmarkId::from_parameter(occupancy), &occupancy, |b, _| {
            b.iter(|| {
                let victim = hash(cursor % occupancy as u64);
                let removed = budget.remove(&victim);
                budget.insert(victim, priority(cursor, 1_000), TX_SIZE);
                cursor += 1;
                removed
            })
        });
    }
    group.finish();
}

fn reconcile(c: &mut Criterion) {
    let mut group = c.benchmark_group("validity_budget/reconcile");
    for occupancy in OCCUPANCIES {
        group.throughput(Throughput::Elements(occupancy as u64));
        // One percent of the residents left reth without the wrapper seeing it.
        let stale_every = 100;
        let pooled: HashSet<TxHash> =
            (0..occupancy as u64).filter(|index| index % stale_every != 0).map(hash).collect();
        group.bench_with_input(BenchmarkId::from_parameter(occupancy), &occupancy, |b, _| {
            b.iter_batched(
                || filled(occupancy, SubPoolLimit::new(occupancy, usize::MAX)),
                |mut budget| {
                    let stale: Vec<TxHash> = budget
                        .tracked_hashes()
                        .into_iter()
                        .filter(|hash| !pooled.contains(hash))
                        .collect();
                    let released = stale.iter().filter(|hash| budget.remove(hash)).count();
                    black_box(released)
                },
                BatchSize::LargeInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, admission, release, reconcile);
criterion_main!(benches);
