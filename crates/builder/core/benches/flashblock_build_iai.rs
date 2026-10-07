//! Deterministic instruction-count gate for the block build path of both builders.
//!
//! `build_block` builds one block of ten flashblocks for a [`FlashblockWorkload`] through the
//! production flashblocks loop (`execute_best_transactions`, per-flashblock `build_block`, and
//! the finalizing state-root `build_block`). `build_native_block` builds the same workload
//! through the native payload builder that serves Denim blocks: one pass over the pool and the
//! state root. Both run with the file transaction event writer enabled, as a production builder
//! runs. Fixture construction (pool, signed transactions, MDBX genesis)
//! runs in the unmeasured setup phase.
//!
//! Callgrind counts only the thread that runs the benchmark function, which is the builder
//! thread: event serialization is counted, the background event-writer thread's file I/O is
//! not. `etc/scripts/ci/builder_gate_check.py` compares these counts against the pinned
//! budgets in `etc/benchmarks/builder-gate-budgets.json` and fails the gate when one is
//! exceeded. See `docs/builder-performance-gate.md`.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use base_builder_core::{
    FlashblockBlockOutcome,
    test_utils::{FlashblockWorkload, FlashblockWorkloadFixture, NativeBlockOutcome},
};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

/// Installs the file event writer and returns the workload for `scenario`.
fn workload(scenario: &str) -> FlashblockWorkload {
    let workload = FlashblockWorkload::by_name(scenario).expect("scenario is in the matrix");
    let events =
        std::env::temp_dir().join(format!("builder-gate-{scenario}-{}.jsonl", std::process::id()));
    FlashblockWorkload::install_file_event_writer(&events).expect("event writer initializes");
    workload
}

/// Materializes `scenario` for the flashblocks builder, outside the measured region.
fn fixture(scenario: &str) -> FlashblockWorkloadFixture {
    FlashblockWorkloadFixture::new(workload(scenario))
}

/// Materializes `scenario` for the native builder, outside the measured region.
fn native_fixture(scenario: &str) -> FlashblockWorkloadFixture {
    FlashblockWorkloadFixture::new_native(workload(scenario))
}

#[library_benchmark]
#[bench::transfers(args = ("transfers"), setup = fixture)]
#[bench::resting_backlog(args = ("resting_backlog"), setup = fixture)]
#[bench::resting_backlog_multi_predicate(
    args = ("resting_backlog_multi_predicate"),
    setup = fixture
)]
#[bench::resting_backlog_shared_state(args = ("resting_backlog_shared_state"), setup = fixture)]
#[bench::wake_rescan(args = ("wake_rescan"), setup = fixture)]
#[bench::backlog_growth(args = ("backlog_growth"), setup = fixture)]
#[bench::congested(args = ("congested"), setup = fixture)]
#[bench::satisfied_validity(args = ("satisfied_validity"), setup = fixture)]
fn build_block(
    mut fixture: FlashblockWorkloadFixture,
) -> (FlashblockBlockOutcome, FlashblockWorkloadFixture) {
    let outcome = fixture.run_block().expect("in-memory block builds");
    assert_eq!(outcome.included, fixture.workload.expected_included());
    // Returning the fixture defers database teardown past the measured region.
    (black_box(outcome), fixture)
}

#[library_benchmark]
#[bench::transfers(args = ("transfers"), setup = native_fixture)]
#[bench::resting_backlog(args = ("resting_backlog"), setup = native_fixture)]
#[bench::resting_backlog_multi_predicate(
    args = ("resting_backlog_multi_predicate"),
    setup = native_fixture
)]
#[bench::resting_backlog_shared_state(
    args = ("resting_backlog_shared_state"),
    setup = native_fixture
)]
#[bench::wake_rescan(args = ("wake_rescan"), setup = native_fixture)]
#[bench::backlog_growth(args = ("backlog_growth"), setup = native_fixture)]
#[bench::congested(args = ("congested"), setup = native_fixture)]
#[bench::satisfied_validity(args = ("satisfied_validity"), setup = native_fixture)]
fn build_native_block(
    mut fixture: FlashblockWorkloadFixture,
) -> (NativeBlockOutcome, FlashblockWorkloadFixture) {
    let outcome = fixture.run_native_block().expect("native block builds");
    assert_eq!(outcome.included, fixture.workload.expected_included());
    // Returning the fixture defers database teardown past the measured region.
    (black_box(outcome), fixture)
}

library_benchmark_group!(
    name = flashblock_build;
    benchmarks = build_block, build_native_block
);

main!(library_benchmark_groups = flashblock_build);
