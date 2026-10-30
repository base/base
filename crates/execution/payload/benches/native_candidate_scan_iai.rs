//! Deterministic instruction-count benchmark for the native payload builder's candidate loop,
//! `BasePayloadBuilderCtx::execute_best_transactions`.
//!
//! The `iai-callgrind` counterpart to the `fits` and `saturated` arms of
//! `native_candidate_scan.rs`, over the same fixture. The native builder is the production block
//! builder after the Denim network upgrade, and its loop body runs once per candidate transaction
//! in every 200 ms block (about 10^9 candidates a day against about 10^7 inclusions; see
//! `docs/guides/HOT_PATH_PERFORMANCE_REVIEW.md`).
//!
//! Building the candidates, the funded state, and the block builder, and applying pre-execution
//! changes and sequencer transactions, happens in iai's unmeasured setup. The scan's inputs are
//! handed back so they drop outside the counted region: only the candidate scan is counted.
//!
//! - `fits`: 300 candidates that all fit and execute, so the count is dominated by transfer
//!   execution and commit.
//! - `saturated`: 1,000 candidates, of which the block has room for about 10%; the rest are
//!   rejected by the block-limits check, the full-block regime where candidates far outnumber
//!   inclusions.
//!
//! Each is a few milliseconds natively, and large enough that the few thousand instructions of
//! allocator noise between runs stay well under 0.1% of the count.
//!
//! The teardown pins the candidates yielded and the transfers, gas, and DA bytes included. They
//! are fixed by the workload, so a mismatch means the loop now selects or scans differently, and
//! the instruction count is not comparable with the base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

#[path = "fixtures/native_candidates.rs"]
mod fixture;

use std::{any::Any, hint::black_box, rc::Rc};

use base_common_consensus::Predeploys;
use base_execution_payload_builder::builder::ExecutionInfo;
use fixture::{Candidates, TRANSFER_GAS, TX_GAS_LIMIT, candidate, context, funded_provider};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_evm::execute::BlockBuilder;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::StateProvider;

/// Estimated DA size of each candidate transfer, in bytes: the estimator's minimum transaction
/// size, since a plain transfer compresses below it.
const DA_BYTES_PER_TRANSFER: u64 = 100;

/// How much of the candidate set the block has room for.
#[derive(Debug, Clone, Copy)]
enum Regime {
    Fits,
    Saturated,
}

impl Regime {
    /// Candidates per scan.
    const fn candidates(self) -> u64 {
        match self {
            Self::Fits => 300,
            Self::Saturated => 1_000,
        }
    }

    /// Block gas limit for the candidate transfers, which each reserve [`TX_GAS_LIMIT`].
    const fn block_gas_limit(self) -> u64 {
        match self {
            // Room for every candidate's gas-limit reservation.
            Self::Fits => self.candidates() * TX_GAS_LIMIT,
            // Room for the reservation of one candidate past 10% of the candidates' executed gas.
            Self::Saturated => TX_GAS_LIMIT + self.candidates() / 10 * TRANSFER_GAS,
        }
    }

    /// Transfers the scan includes.
    const fn included(self) -> u64 {
        match self {
            Self::Fits => self.candidates(),
            Self::Saturated => self.candidates() / 10 + 1,
        }
    }
}

/// Workload outcomes of one candidate scan.
#[derive(Debug, PartialEq, Eq)]
struct ScanTotals {
    cancelled: bool,
    yielded: usize,
    gas_used: u64,
    da_bytes_used: u64,
}

/// A prepared candidate scan. Calling it runs only `execute_best_transactions` and returns the
/// scan's state alongside the totals, so that state drops outside the counted region.
type Scan = Box<dyn FnOnce() -> (ScanTotals, Box<dyn Any>)>;

/// Builds the candidates, state, and block builder outside the counted region.
fn setup(regime: Regime) -> (Regime, Scan) {
    let candidates: Vec<_> = (0..regime.candidates()).map(|i| candidate(i, TX_GAS_LIMIT)).collect();
    let provider = funded_provider(&candidates);
    // The block builder borrows the context and the state, and the prepared scan must own the
    // builder, so both are leaked for the life of the single-run bench process.
    let ctx: &'static _ = Box::leak(Box::new(context(regime.block_gas_limit())));
    let db = Box::leak(Box::new(
        State::builder()
            .with_database(StateProviderDatabase::new(provider.into_evm_state_provider()))
            .with_bundle_update()
            .build(),
    ));
    db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
    let mut builder = ctx.block_builder(db).expect("block builder");
    builder.apply_pre_execution_changes().expect("pre-execution changes");
    let mut info: ExecutionInfo = ctx.execute_sequencer_transactions(&mut builder).expect("seq");
    let yielded = Rc::default();
    // The pool keeps its own reference to each candidate, so dropping one in the scan is a
    // reference-count decrement, as in production.
    let transactions =
        Candidates { queued: candidates.iter().cloned().collect(), yielded: Rc::clone(&yielded) };
    let scan: Scan = Box::new(move || {
        let cancelled = ctx
            .execute_best_transactions(&mut info, &mut builder, transactions, None)
            .expect("scan");
        let totals = ScanTotals {
            cancelled: cancelled.is_some(),
            yielded: yielded.get(),
            gas_used: info.cumulative_gas_used,
            da_bytes_used: info.cumulative_da_bytes_used,
        };
        (totals, Box::new((info, builder, candidates)))
    });
    (regime, scan)
}

/// Checks the scan's outcomes against the workload's documented counts.
fn check_outcomes((regime, totals, _): (Regime, ScanTotals, Box<dyn Any>)) {
    let included = regime.included();
    assert_eq!(
        totals,
        ScanTotals {
            cancelled: false,
            yielded: regime.candidates() as usize,
            gas_used: included * TRANSFER_GAS,
            da_bytes_used: included * DA_BYTES_PER_TRANSFER,
        },
        "native_candidate_scan/{regime:?} outcomes changed"
    );
}

#[library_benchmark]
#[bench::fits(args = (Regime::Fits), setup = setup, teardown = check_outcomes)]
#[bench::saturated(args = (Regime::Saturated), setup = setup, teardown = check_outcomes)]
fn scan((regime, scan): (Regime, Scan)) -> (Regime, ScanTotals, Box<dyn Any>) {
    let (totals, state) = scan();
    (regime, black_box(totals), state)
}

library_benchmark_group!(
    name = native_candidate_scan;
    benchmarks = scan
);

main!(library_benchmark_groups = native_candidate_scan);
