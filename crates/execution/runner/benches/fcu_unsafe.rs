//! Latency benchmark for `engine_forkchoiceUpdatedV3` on the unsafe-head path.
//!
//! Per iteration:
//!  1. Build a fresh empty block via FCU-with-attrs + `get_payload` + `new_payload`.
//!     The block lands in the engine's tree but is not yet canonical.
//!  2. Time only the canonical FCU that promotes that block to the head, with
//!     `safe`/`finalized` parked at genesis to mirror the production unsafe-head
//!     case where consensus advances the head ahead of the safe head.
//!
//! The harness talks to a real reth node over IPC, so the measurement covers
//! the full reth FCU handler — validation, tree update, canonical-chain update,
//! state-root checks, persistence — over a real JSON-RPC transport.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p base-node-runner --bench fcu_unsafe
//! ```
//!
//! Set `FCU_BENCH_VERBOSE=1` for tracing output during the run.

use std::{
    sync::Once,
    time::{Duration, Instant},
};

use alloy_provider::Provider;
use alloy_rpc_types::BlockNumberOrTag;
use base_node_runner::test_utils::{PreparedBlock, TestHarness};
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

fn fcu_unsafe_benches(c: &mut Criterion) {
    init_bench_tracing();

    let runtime = Runtime::new().expect("tokio runtime should start");
    let harness = runtime.block_on(async {
        TestHarness::new().await.expect("fcu_unsafe bench: harness should start")
    });
    let genesis_hash = runtime
        .block_on(async {
            harness
                .provider()
                .get_block_by_number(BlockNumberOrTag::Number(0))
                .await
                .expect("genesis lookup should succeed")
        })
        .expect("genesis block should exist")
        .header
        .hash;

    let mut group = c.benchmark_group("engine_forkchoiceUpdatedV3");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("advance_unsafe_head", |b| {
        b.iter_custom(|iters| {
            runtime.block_on(async {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let PreparedBlock { new_block_hash, .. } = harness
                        .prepare_unsafe_block(vec![])
                        .await
                        .expect("prepare_unsafe_block should succeed");

                    let start = Instant::now();
                    let result = harness
                        .engine()
                        .update_forkchoice(genesis_hash, new_block_hash, None)
                        .await
                        .expect("forkchoice update should succeed");
                    total += start.elapsed();

                    assert!(
                        !result.payload_status.status.is_invalid(),
                        "engine reported invalid status during fcu: {result:?}"
                    );
                    assert_eq!(
                        result.payload_status.latest_valid_hash,
                        Some(new_block_hash),
                        "engine did not promote new block to head"
                    );
                }
                total
            })
        });
    });

    group.finish();
}

fn init_bench_tracing() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let verbose = std::env::var_os("FCU_BENCH_VERBOSE").is_some();
        let default_level = if verbose { LevelFilter::INFO } else { LevelFilter::ERROR };

        let mut filter =
            EnvFilter::builder().with_default_directive(default_level.into()).from_env_lossy();

        for directive in ["reth_tasks=off", "reth_node_builder::launch::common=off"] {
            if let Ok(directive) = directive.parse() {
                filter = filter.add_directive(directive);
            }
        }

        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    });
}

criterion_group!(benches, fcu_unsafe_benches);
criterion_main!(benches);
