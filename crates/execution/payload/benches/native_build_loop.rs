//! End-to-end benchmark of the native payload builder's selection loop across consecutive blocks.
//!
//! Each iteration builds [`BLOCKS`] one-shot blocks, the post-Denim shape, through
//! `BasePayloadBuilderCtx::execute_best_transactions`. The pool holds `resting` validity
//! transactions whose balance predicates never become satisfied, plus enough plain transfers to
//! fill every block. Transaction events are enabled, as in production.
//!
//! With resting predicates off, every block re-evaluates every resting transaction. With them
//! enforced, only the first block evaluates them and later blocks hold them back unevaluated.
//! Every block builds on the same parent state, so the carried set stays valid from block to
//! block.

#[path = "fixtures/native_build_loop.rs"]
mod fixture;

use std::{hint::black_box, time::Duration};

use alloy_primitives::{B256, U256};
use base_common_consensus::Predeploys;
use base_execution_payload_builder::{
    DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD, RestingPredicateMode, RestingPredicates,
    builder::ExecutionInfo,
};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use criterion::{Criterion, criterion_group, criterion_main};
use fixture::{
    Ctx, FILLER_PER_BLOCK, Pool, parkable, payload_context, populated_rejection_cache, resting_pool,
};
use reth_evm::execute::{BlockBuilder, BlockExecutor};
use reth_revm::db::State;
use revm::database::{InMemoryDB, TransitionState};

/// Consecutive blocks built per iteration.
const BLOCKS: u64 = 10;
/// Resting validity transaction counts to measure.
const RESTING_COUNTS: [usize; 2] = [20_000, 40_000];
/// Rejection-cache populations to measure: empty, and the order of magnitude a running builder
/// holds from rejections in earlier blocks.
const REJECTED_COUNTS: [usize; 2] = [0, 10_000];

/// Runs the selection loop for one block and returns the number of included transactions.
fn run_block(
    ctx: &Ctx,
    pool: &Pool,
    db: &InMemoryDB,
    resting: Option<&mut RestingPredicates>,
) -> usize {
    let mut state = State::builder().with_database(db.clone()).with_bundle_update().build();
    state.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("in-memory load cannot fail");
    let mut builder = ctx.block_builder(&mut state).expect("block builder");
    let mut info = ExecutionInfo::default();
    black_box(
        ctx.execute_best_transactions(&mut info, &mut builder, parkable(pool), resting)
            .expect("in-memory execution cannot fail"),
    );
    builder.executor().receipts().len()
}

/// Builds [`BLOCKS`] consecutive blocks, carrying the resting set when `mode` is enabled.
fn run_blocks(ctx: &Ctx, pool: &Pool, db: &InMemoryDB, mode: RestingPredicateMode) {
    let mut resting = RestingPredicates::new(DEFAULT_PREDICATE_BUCKET_ORDERED_THRESHOLD);
    let mut parent = B256::ZERO;
    for block in 1..=BLOCKS {
        // The production builder passes the pre-block writes; this loop makes none.
        resting.begin_block(parent, block, Some(&TransitionState::default()));
        let included = run_block(ctx, pool, db, mode.is_enabled().then_some(&mut resting));
        assert_eq!(included, FILLER_PER_BLOCK);
        parent = B256::from(U256::from(block));
        resting.end_block(parent);
    }
}

fn native_build_loop_benches(c: &mut Criterion) {
    let events = tempfile::tempdir().expect("temporary event directory");
    GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
        enabled: true,
        ..TransactionEventWriterConfig::disabled(
            TransactionEventProducer::BaseBuilder,
            "bench",
            events.path().join("events.jsonl"),
        )
    }))
    .expect("event writer initializes");

    let mut group = c.benchmark_group("native_build_loop/resting_predicates");
    group.sample_size(10).measurement_time(Duration::from_secs(20));
    for resting in RESTING_COUNTS {
        let (pool, db) = resting_pool(resting);
        for rejected in REJECTED_COUNTS {
            // The empty-cache case keeps its original name so earlier results stay comparable.
            let rejected_suffix =
                if rejected == 0 { String::new() } else { format!("/rejected={rejected}") };
            for mode in [RestingPredicateMode::Off, RestingPredicateMode::Enforce] {
                let ctx = payload_context(mode, populated_rejection_cache(rejected));
                group.bench_function(
                    format!("resting={resting}/blocks={BLOCKS}/mode={mode:?}{rejected_suffix}"),
                    |b| b.iter(|| run_blocks(&ctx, &pool, &db, mode)),
                );
            }
        }
    }
    group.finish();
}

criterion_group!(benches, native_build_loop_benches);
criterion_main!(benches);
