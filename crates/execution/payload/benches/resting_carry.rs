//! Benchmarks carrying a large set of resting validity transactions across blocks.
//!
//! Under sustained validity traffic a builder can carry over 100k resting members, and an unseen
//! sweep can drop tens of thousands of them at once. Work done in
//! `RestingPredicates::begin_block` runs on the build path before transaction selection, so every
//! millisecond spent there comes out of the block's build budget. This benchmark measures the
//! block-start path and the unseen sweep separately.
//! Before the sweep moved to the end of the block, `begin_block/sweep_block` included it.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench resting_carry
//! ```

use std::{hint::black_box, time::Duration};

use alloy_primitives::{Address, B256, U256, keccak256};
use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, RESTING_UNSEEN_RETAINED_BLOCKS, RestingPredicates,
};
use base_execution_txpool::{ValidityOperator, ValidityPredicate};
use criterion::{BatchSize, BenchmarkId, Criterion};
use revm::{
    database::TransitionState,
    state::{Account, EvmState},
};

/// Members still yielded by the pool, kept by the sweep.
const LIVE_MEMBERS: usize = 75_000;
/// Members no build yielded for longer than the retention window, dropped by the sweep.
const STALE_MEMBERS: usize = 45_000;
/// Distinct contracts the storage predicates read.
const CONTRACTS: u64 = 64;
/// Distinct accounts the balance predicates read, so their buckets convert to the ordered form.
const ACCOUNTS: u64 = 256;

/// First block whose start sweeps unseen members.
const SWEEP_BLOCK: u64 = 2 * RESTING_UNSEEN_RETAINED_BLOCKS;

fn tx_hash(i: u64) -> B256 {
    keccak256(i.to_be_bytes())
}

fn block_hash(block: u64) -> B256 {
    keccak256((u64::MAX - block).to_be_bytes())
}

/// Mix of storage-equality predicates (one slot per sender, like a smart-account nonce) and
/// balance thresholds shared by many members.
fn predicate(i: u64) -> ValidityPredicate {
    if i.is_multiple_of(4) {
        ValidityPredicate::Balance {
            address: Address::with_last_byte((i % ACCOUNTS) as u8),
            op: ValidityOperator::GreaterThanOrEqual,
            value: U256::from(1_000_000 + i),
        }
    } else {
        let mut address = [0u8; 20];
        address[..8].copy_from_slice(&(i % CONTRACTS).to_be_bytes());
        ValidityPredicate::Storage {
            address: Address::from(address),
            slot: U256::from(i),
            mask: U256::MAX,
            op: ValidityOperator::Equal,
            value: U256::from(i + 1),
        }
    }
}

/// Members rested and released again during a past burst, which leave the set's maps at their
/// peak capacity.
const BURST_MEMBERS: u64 = 1_000_000;

/// Returns a set that finished `last_block`, holding `STALE_MEMBERS` last seen in block 1 and
/// `LIVE_MEMBERS` last seen in `last_block`. With `burst`, the set first grew to
/// [`BURST_MEMBERS`] in block 1.
fn large_set(last_block: u64, burst: bool) -> RestingPredicates {
    let empty = TransitionState::default();
    let mut resting = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
    resting.begin_block(B256::ZERO, 1, Some(&empty));
    if burst {
        let first = (STALE_MEMBERS + LIVE_MEMBERS) as u64;
        for i in first..first + BURST_MEMBERS {
            resting.rest(tx_hash(i), predicate(i));
        }
        for i in first..first + BURST_MEMBERS {
            resting.remove(tx_hash(i));
        }
    }
    for i in 0..STALE_MEMBERS as u64 {
        resting.rest(tx_hash(i), predicate(i));
    }
    resting.end_block(block_hash(1));
    for block in 2..=last_block {
        resting.begin_block(block_hash(block - 1), block, Some(&empty));
        if block == last_block {
            for i in STALE_MEMBERS as u64..(STALE_MEMBERS + LIVE_MEMBERS) as u64 {
                resting.rest(tx_hash(i), predicate(i));
            }
        }
        resting.end_block(block_hash(block));
    }
    assert_eq!(resting.len(), STALE_MEMBERS + LIVE_MEMBERS);
    resting
}

fn bench_begin_block(c: &mut Criterion) {
    let empty = TransitionState::default();
    let mut group = c.benchmark_group("resting_carry/begin_block");
    group.sample_size(10).measurement_time(Duration::from_secs(20));
    for (label, block, burst) in [
        ("regular", SWEEP_BLOCK - 1, false),
        ("sweep_block", SWEEP_BLOCK, false),
        ("sweep_block_after_burst", SWEEP_BLOCK, true),
    ] {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_batched(
                || large_set(block - 1, burst),
                |mut resting| {
                    let carry_over =
                        resting.begin_block(block_hash(block - 1), block, Some(&empty));
                    black_box(carry_over);
                    resting
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// The unseen sweep itself, which now runs when a sweep block finishes, off the build path.
fn bench_end_of_block_sweep(c: &mut Criterion) {
    let empty = TransitionState::default();
    let mut group = c.benchmark_group("resting_carry/end_of_block_sweep");
    group.sample_size(10).measurement_time(Duration::from_secs(20));
    for (label, burst) in [("steady", false), ("after_burst", true)] {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_batched(
                || {
                    let mut resting = large_set(SWEEP_BLOCK - 1, burst);
                    resting.begin_block(block_hash(SWEEP_BLOCK - 1), SWEEP_BLOCK, Some(&empty));
                    resting
                },
                |mut resting| {
                    assert_eq!(resting.sweep_unseen(), STALE_MEMBERS);
                    resting
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// One write wakes thousands of resting transactions. Compare the legacy single-set path,
/// a grouped waiter elsewhere, and grouped waiters reached by multiple matching comparisons.
fn bench_shared_key_wake(c: &mut Criterion) {
    const WAITERS: u64 = 4096;
    let watched = Address::with_last_byte(1);
    let predicate = |address, op| ValidityPredicate::Balance { address, op, value: U256::ONE };
    let mut account = Account::default();
    account.info.balance = U256::ONE;
    let change = EvmState::from_iter([(watched, account)]);
    let mut group = c.benchmark_group("resting_carry/shared_key_wake");
    for label in ["flat", "unrelated_group", "overlapping_groups"] {
        let mut resting = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
        for i in 0..WAITERS {
            let first = predicate(watched, ValidityOperator::Equal);
            if label == "overlapping_groups" && i.is_multiple_of(2) {
                resting.rest_grouped(
                    tx_hash(i),
                    first,
                    vec![predicate(watched, ValidityOperator::GreaterThanOrEqual)],
                );
            } else {
                resting.rest(tx_hash(i), first);
            }
        }
        if label == "unrelated_group" {
            resting.rest_grouped(
                tx_hash(WAITERS),
                predicate(Address::with_last_byte(2), ValidityOperator::Equal),
                vec![predicate(Address::with_last_byte(3), ValidityOperator::Equal)],
            );
        }
        assert_eq!(resting.woken_by_state(&change).len(), WAITERS as usize);
        group.bench_function(label, |b| b.iter(|| resting.woken_by_state(black_box(&change))));
    }
    group.finish();
}

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    bench_shared_key_wake(&mut criterion);
    bench_begin_block(&mut criterion);
    bench_end_of_block_sweep(&mut criterion);
    criterion.final_summary();
}
