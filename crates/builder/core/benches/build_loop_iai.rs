//! Deterministic instruction-count benchmark for the `build_loop/mainnet_block` workload.
//!
//! The `iai-callgrind` counterpart to the mainnet arm of `build_loop.rs`, with observability
//! off as in its `default` profile. Building the fixture, the fresh state, and the rejection
//! cache happens in iai's unmeasured setup, and the inputs are handed back so they drop
//! outside the counted region: only [`MainnetBlock::build`] is counted.
//!
//! The teardown pins the block's outcome counts. They are fixed by the workload, so a mismatch
//! means the builder now selects different transactions, and the instruction count is not
//! comparable with the base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

#[path = "fixtures/mainnet_block.rs"]
mod fixture;

use std::hint::black_box;

use base_builder_core::RejectionCache;
use fixture::{BenchKnobs, BlockTotals, DelayedDb, MainnetBlock, Workload};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_revm::State;

/// A block ready to build, plus the workload to build it under.
type Input = (MainnetBlock, State<DelayedDb>, RejectionCache, Workload);

/// Builds the fixture and the per-block state outside the counted region.
fn setup(workload: Workload) -> Input {
    // Every knob off keeps the instruction count deterministic.
    let block = MainnetBlock::with_knobs(BenchKnobs::default());
    let state = block.fresh_state();
    let rejection_cache = block.rejection_cache();
    (block, state, rejection_cache, workload)
}

/// Checks the block's outcomes against the workload's documented counts.
fn check_outcomes((totals, (block, _, _, workload)): (BlockTotals, Input)) {
    let (considered, included) = match workload {
        Workload::Off => (31_537, 187),
        Workload::Enforce => (3_357, 187),
        Workload::EnforceIngestion => (523, 187),
    };
    assert_eq!(
        (totals.considered, totals.included),
        (considered, included),
        "build_loop/mainnet_block mode={} selection changed: {totals:?}",
        workload.label()
    );
    assert_eq!(
        totals,
        block.expected_totals(workload),
        "block outcomes match the documented workload"
    );
}

#[library_benchmark]
#[bench::off(args = (Workload::Off), setup = setup, teardown = check_outcomes)]
#[bench::enforce(args = (Workload::Enforce), setup = setup, teardown = check_outcomes)]
#[bench::enforce_ingestion(
    args = (Workload::EnforceIngestion),
    setup = setup,
    teardown = check_outcomes
)]
fn mainnet_block(mut input: Input) -> (BlockTotals, Input) {
    let totals = input.0.build(&mut input.1, &input.2, input.3);
    (black_box(totals), input)
}

library_benchmark_group!(
    name = build_loop;
    benchmarks = mainnet_block
);

main!(library_benchmark_groups = build_loop);
