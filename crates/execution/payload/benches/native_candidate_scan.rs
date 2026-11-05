//! End-to-end benchmark of the native payload builder's candidate loop,
//! `BasePayloadBuilderCtx::execute_best_transactions`.
//!
//! Each iteration builds fresh state, applies pre-execution changes and sequencer transactions,
//! then times only the candidate scan. Candidates are EIP-1559 transfers from distinct funded
//! senders, built at a post-Jovian mainnet timestamp so the DA-footprint limit is active.
//!
//! - `fits/N`: every candidate fits and executes.
//! - `saturated/N`: the block has room for about 10% of the candidates; the rest are rejected by
//!   the block-limits check. This is the full-block regime, where candidates far outnumber
//!   inclusions.
//! - `oversized/N`: every candidate's gas limit exceeds the block's, so all are rejected by the
//!   block-limits check without executing.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench native_candidate_scan
//! ```

#[path = "fixtures/native_candidates.rs"]
mod fixture;

use std::{hint::black_box, time::Duration};

use base_common_consensus::Predeploys;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::builder::{BasePayloadBuilderCtx, ExecutionInfo};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use fixture::{
    Candidate, Candidates, TRANSFER_GAS, TX_GAS_LIMIT, candidate, context, funded_provider,
};
use reth_evm::execute::BlockBuilder;
use reth_revm::{database::StateProviderDatabase, db::State, test_utils::StateProviderTest};
use reth_storage_api::StateProvider;

/// Times one candidate scan over fresh state, returning its duration and the number of
/// transfers included.
fn scan(
    ctx: &BasePayloadBuilderCtx<BaseEvmConfig, BaseChainSpec>,
    provider: &StateProviderTest,
    candidates: &[Candidate],
) -> (Duration, u64) {
    let mut db = State::builder()
        .with_database(StateProviderDatabase::new(provider.into_evm_state_provider()))
        .with_bundle_update()
        .build();
    db.load_cache_account(Predeploys::L1_BLOCK_INFO).expect("L1 block info must load");
    let mut builder = ctx.block_builder(&mut db).expect("block builder");
    builder.apply_pre_execution_changes().expect("pre-execution changes");
    let mut info: ExecutionInfo = ctx.execute_sequencer_transactions(&mut builder).expect("seq");
    let transactions =
        Candidates { queued: candidates.iter().cloned().collect(), yielded: Default::default() };

    let start = std::time::Instant::now();
    let cancelled =
        ctx.execute_best_transactions(&mut info, &mut builder, transactions, None).expect("scan");
    let elapsed = start.elapsed();
    black_box(cancelled);
    (elapsed, info.cumulative_gas_used / TRANSFER_GAS)
}

fn bench_native_candidate_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("native_candidate_scan");
    for count in [100u64, 1_000] {
        // (block gas limit, candidate gas limit, transfers included).
        // Room for every candidate's gas-limit reservation.
        let fits = (count * TX_GAS_LIMIT, TX_GAS_LIMIT, count);
        // Room for the reservation of one candidate past 10% of the candidates' executed gas.
        let tenth = count / 10;
        let saturated = (TX_GAS_LIMIT + tenth * TRANSFER_GAS, TX_GAS_LIMIT, tenth + 1);
        // Every candidate reserves more gas than the block holds.
        let oversized = (1_000_000, 2_000_000, 0);
        for (name, (block_gas_limit, tx_gas_limit, included)) in
            [("fits", fits), ("saturated", saturated), ("oversized", oversized)]
        {
            let candidates: Vec<_> = (0..count).map(|i| candidate(i, tx_gas_limit)).collect();
            let provider = funded_provider(&candidates);
            let ctx = context(block_gas_limit);
            assert_eq!(scan(&ctx, &provider, &candidates).1, included, "{name}/{count}");
            group.bench_with_input(BenchmarkId::new(name, count), &candidates, |b, candidates| {
                b.iter_custom(|iters| {
                    (0..iters).map(|_| scan(&ctx, &provider, candidates).0).sum::<Duration>()
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_native_candidate_scan);
criterion_main!(benches);
