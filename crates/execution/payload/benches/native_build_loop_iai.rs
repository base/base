//! Deterministic instruction-count benchmark for the native payload builder's selection loop
//! across consecutive blocks.
//!
//! The `iai-callgrind` counterpart to the `resting=*/mode=Off` and `mode=Enforce` arms of
//! `native_build_loop.rs`, over the same fixture at a callgrind-sized scale: [`BLOCKS`] one-shot
//! blocks, the post-Denim shape, through `BasePayloadBuilderCtx::execute_best_transactions`, with
//! [`RESTING`] never-satisfied validity transactions ahead of a block of filler transfers and
//! transaction events enabled, as in production. The native builder is the production block
//! builder after the Denim network upgrade; its loop body runs once per candidate in every 200 ms
//! block (about 10^9 candidates a day; see `docs/guides/HOT_PATH_PERFORMANCE_REVIEW.md`), and
//! carried resting candidates make up most of a block's validity candidates.
//!
//! - `off`: every block re-evaluates every resting transaction and parks it again.
//! - `enforce`: the first block evaluates them; later blocks hold the carried members back
//!   unevaluated.
//!
//! Building the pool, each block's state, block builder, and best-transaction iterator, and
//! starting the event writer, happens in iai's unmeasured setup, and the blocks' state is handed
//! back so it drops outside the counted region. Only the per-block resting carry-over
//! (`RestingPredicates::begin_block`/`end_block`) and the candidate scans are counted. The event
//! writer thread's serialization runs on its own thread and is not counted; the builder's
//! per-event work on the build thread is. The wall-clock predicate-evaluation budget is lifted,
//! because callgrind's slowdown would otherwise cut evaluation off at a timing-dependent
//! candidate; natively these evaluations fit well within it.
//!
//! The teardown pins each mode's candidates considered, transactions included, candidates
//! deferred (parked), and resting members carried into later blocks. They are fixed by the
//! workload, so a mismatch means the builder now selects or carries differently, and the
//! instruction count is not comparable with the base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

#[path = "fixtures/native_build_loop.rs"]
mod fixture;

use std::{any::Any, cell::Cell, hint::black_box, rc::Rc, time::Duration};

use alloy_primitives::{Address, B256, TxHash, U256};
use base_common_consensus::Predeploys;
use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, ParkableBestPayloadTransactions,
    ParkablePayloadTransactions, RestingPredicateMode, RestingPredicates, builder::ExecutionInfo,
};
use base_execution_txpool::BasePooledTransaction;
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use fixture::{
    FILLER_PER_BLOCK, parkable, payload_context, populated_rejection_cache, resting_pool,
};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_payload_util::PayloadTransactions;
use reth_revm::db::State;
use revm::database::TransitionState;

/// Consecutive blocks built per run.
const BLOCKS: u64 = 5;
/// Never-satisfied resting validity transactions in the pool.
const RESTING: usize = 5_000;
/// Event queue capacity, above the most events a run emits, so no event is dropped for
/// backpressure and the build thread's work does not depend on writer-thread scheduling.
const EVENT_QUEUE_CAPACITY: usize = 1 << 17;

/// Candidate lifecycle counts across every block of a run.
#[derive(Debug, Default)]
struct Lifecycle {
    considered: Cell<usize>,
    deferred: Cell<usize>,
}

/// Counts the candidates the builder considers and parks, forwarding to the production iterator.
struct Counted {
    inner: ParkableBestPayloadTransactions<BasePooledTransaction>,
    lifecycle: Rc<Lifecycle>,
}

impl PayloadTransactions for Counted {
    type Transaction = <ParkableBestPayloadTransactions<BasePooledTransaction> as PayloadTransactions>::Transaction;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        let next = self.inner.next(ctx);
        let considered = &self.lifecycle.considered;
        considered.set(considered.get() + usize::from(next.is_some()));
        next
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);
    }
}

impl ParkablePayloadTransactions for Counted {
    type Pooled = BasePooledTransaction;

    fn park_current(&mut self) {
        let deferred = &self.lifecycle.deferred;
        deferred.set(deferred.get() + 1);
        self.inner.park_current();
    }

    fn mark_current_committed(&mut self) {
        self.inner.mark_current_committed();
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.discard_parked(transaction_hash)
    }
}

/// Workload outcomes of one run.
#[derive(Debug, PartialEq, Eq)]
struct BlockTotals {
    considered: usize,
    included: usize,
    deferred: usize,
    carried: usize,
}

/// Prepared blocks. Calling it runs only the carry-over and candidate scans, and returns the
/// blocks' state alongside the totals so that state drops outside the counted region.
type Blocks = Box<dyn FnOnce() -> (BlockTotals, Box<dyn Any>)>;

/// Builds the pool, every block's state, builder, and iterator, and the event writer outside the
/// counted region.
fn setup(mode: RestingPredicateMode) -> (RestingPredicateMode, Blocks) {
    // The writer thread appends here; the directory is removed with the returned state.
    let events = tempfile::tempdir().expect("temporary event directory");
    GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
        enabled: true,
        queue_capacity: EVENT_QUEUE_CAPACITY,
        ..TransactionEventWriterConfig::disabled(
            TransactionEventProducer::BaseBuilder,
            "bench",
            events.path().join("events.jsonl"),
        )
    }))
    .expect("event writer initializes");

    let (pool, db) = resting_pool(RESTING);
    // Each block builder borrows the context and its state, and the prepared run must own the
    // builders, so both are leaked for the life of the single-run bench process.
    let mut ctx = payload_context(mode, populated_rejection_cache(0));
    // Callgrind runs tens of times slower than native code, so the wall-clock predicate
    // evaluation budget would stop evaluation at a timing-dependent candidate. Natively this
    // workload evaluates every resting transaction within the budget, so lift it to match.
    ctx.builder_config.predicate_eval_hard_cutoff = Duration::MAX;
    let ctx: &'static _ = Box::leak(Box::new(ctx));
    let lifecycle = Rc::new(Lifecycle::default());
    let blocks: Vec<_> = (0..BLOCKS)
        .map(|_| {
            let state = Box::leak(Box::new(
                State::builder().with_database(db.clone()).with_bundle_update().build(),
            ));
            state
                .load_cache_account(Predeploys::L1_BLOCK_INFO)
                .expect("in-memory load cannot fail");
            let builder = ctx.block_builder(state).expect("block builder");
            let transactions = Counted { inner: parkable(&pool), lifecycle: Rc::clone(&lifecycle) };
            (builder, ExecutionInfo::default(), transactions)
        })
        .collect();
    let mut resting = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);

    let run: Blocks = Box::new(move || {
        let mut included = 0;
        let mut carried = 0;
        let mut finished = Vec::with_capacity(blocks.len());
        let mut parent = B256::ZERO;
        for (block, (mut builder, mut info, transactions)) in (1..).zip(blocks) {
            // The production builder passes the pre-block writes; these blocks make none.
            carried +=
                resting.begin_block(parent, block, Some(&TransitionState::default())).carried;
            black_box(
                ctx.execute_best_transactions(
                    &mut info,
                    &mut builder,
                    transactions,
                    mode.is_enabled().then_some(&mut resting),
                )
                .expect("in-memory execution cannot fail"),
            );
            included += builder.executor().receipts().len();
            parent = B256::from(U256::from(block));
            resting.end_block(parent);
            finished.push((builder, info));
        }
        let totals = BlockTotals {
            considered: lifecycle.considered.get(),
            included,
            deferred: lifecycle.deferred.get(),
            carried,
        };
        (totals, Box::new((finished, resting, pool, events)))
    });
    (mode, run)
}

/// Checks the run's outcomes against the workload's documented counts.
fn check_outcomes((mode, totals, _): (RestingPredicateMode, BlockTotals, Box<dyn Any>)) {
    let blocks = BLOCKS as usize;
    // Every block yields every resting transaction and the filler, includes the filler, and
    // parks every resting transaction, whether evaluated or held back.
    let carried = match mode {
        RestingPredicateMode::Off => 0,
        // Every member rested in the first block carries into each later block.
        RestingPredicateMode::Enforce => RESTING * (blocks - 1),
        RestingPredicateMode::Shadow => unimplemented!("the build loop measures Off and Enforce"),
    };
    assert_eq!(
        totals,
        BlockTotals {
            considered: (RESTING + FILLER_PER_BLOCK) * blocks,
            included: FILLER_PER_BLOCK * blocks,
            deferred: RESTING * blocks,
            carried,
        },
        "native_build_loop mode={mode:?} outcomes changed"
    );
}

#[library_benchmark]
#[bench::off(args = (RestingPredicateMode::Off), setup = setup, teardown = check_outcomes)]
#[bench::enforce(args = (RestingPredicateMode::Enforce), setup = setup, teardown = check_outcomes)]
fn resting_predicates(
    (mode, run): (RestingPredicateMode, Blocks),
) -> (RestingPredicateMode, BlockTotals, Box<dyn Any>) {
    let (totals, state) = run();
    (mode, black_box(totals), state)
}

library_benchmark_group!(
    name = native_build_loop;
    benchmarks = resting_predicates
);

main!(library_benchmark_groups = native_build_loop);
