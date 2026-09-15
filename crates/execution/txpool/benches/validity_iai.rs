//! Deterministic instruction-count benchmarks for validity-predicate handling.
//!
//! This is the `iai-callgrind` counterpart to `validity.rs`. Instead of timing
//! wall-clock on a shared runner — where cache and scheduling noise produce
//! bimodal, untrustworthy deltas — it runs the same routines under Valgrind's
//! Cachegrind simulator and counts retired instructions. The count is identical
//! on every run of the same code, so a base-vs-head delta reflects a code change
//! and nothing else.
//!
//! [`ValidityPredicate::validate_batch`], [`ValidityPredicate::block_expiry_bound`],
//! and [`ValidityPredicate::is_batch_expired`] are pure, allocation-free,
//! cache-resident scans over a small predicate slice, so their instruction count
//! is a faithful proxy for real wall-clock time and belongs in the deterministic
//! advisory subset.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use alloy_primitives::{Address, U256};
use base_execution_txpool::{
    DEFAULT_MAX_VALIDITY_PREDICATES, PredicateContext, ValidityOperator, ValidityPredicate,
};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

/// Builds a batch of `count` internally valid predicates cycling through all four
/// variants, so `validate_batch` walks and validates every element. Runs in the
/// unmeasured setup phase so allocation is excluded from the counted region.
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

/// Builds a batch of `count` upper-bounded positional predicates with distinct
/// bounds, forcing the expiry checks to fold the tightest bound across every
/// element. Runs in the unmeasured setup phase.
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

#[library_benchmark]
#[benches::sizes(args = [1usize, 8, DEFAULT_MAX_VALIDITY_PREDICATES], setup = mixed_batch)]
fn validate_batch(batch: Vec<ValidityPredicate>) -> bool {
    black_box(
        ValidityPredicate::validate_batch(black_box(&batch), DEFAULT_MAX_VALIDITY_PREDICATES)
            .is_ok(),
    )
}

#[library_benchmark]
#[benches::sizes(args = [1usize, 8, DEFAULT_MAX_VALIDITY_PREDICATES], setup = positional_batch)]
fn block_expiry_bound(batch: Vec<ValidityPredicate>) -> Option<u64> {
    black_box(ValidityPredicate::block_expiry_bound(black_box(&batch)))
}

#[library_benchmark]
#[benches::sizes(args = [1usize, 8, DEFAULT_MAX_VALIDITY_PREDICATES], setup = positional_batch)]
fn is_batch_expired(batch: Vec<ValidityPredicate>) -> bool {
    // A build position that leaves every bound satisfiable, so the check performs
    // the full fold instead of short-circuiting.
    let context = PredicateContext { block_number: 10, flashblock_index: 1 };
    black_box(ValidityPredicate::is_batch_expired(black_box(&batch), black_box(&context)))
}

library_benchmark_group!(
    name = validity;
    benchmarks = validate_batch, block_expiry_bound, is_batch_expired
);

main!(library_benchmark_groups = validity);
