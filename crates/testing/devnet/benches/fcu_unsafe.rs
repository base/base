//! Latency benchmark for native append on the unsafe-head path.
//!
//! Each iteration builds and resolves a fresh block, then times its import and
//! canonicalization in one serialized command. Safety markers remain at genesis.
//! The harness uses the node's in-process execution handle.
//!
//! Run with `cargo bench -p base-testing-devnet --bench fcu_unsafe`.
//! Set `FCU_BENCH_VERBOSE=1` for tracing output during the run.

use std::{
    sync::Once,
    time::{Duration, Instant},
};

use base_common_client_ethereum::Provider;
use base_common_types_payload::ForkchoiceState;
use base_common_types_rpc::BlockNumberOrTag;
use base_testing_devnet::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
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

    let mut group = c.benchmark_group("append_payload");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("advance_unsafe_head", |b| {
        b.iter_custom(|iters| {
            runtime.block_on(async {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let block = harness
                        .prepare_unsafe_block(vec![L1_BLOCK_INFO_DEPOSIT_TX])
                        .await
                        .expect("prepare_unsafe_block should succeed");

                    let start = Instant::now();
                    let result = harness
                        .engine()
                        .execution
                        .driver
                        .append_payload(
                            block.payload,
                            ForkchoiceState {
                                head_block_hash: block.new_block_hash,
                                safe_block_hash: genesis_hash,
                                finalized_block_hash: genesis_hash,
                            },
                        )
                        .await
                        .expect("append should succeed");
                    total += start.elapsed();

                    assert!(result.is_applied(), "engine did not append block: {result:?}");
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

        for directive in ["base_common_runtime=off", "base_node_service::launch::common=off"] {
            if let Ok(directive) = directive.parse() {
                filter = filter.add_directive(directive);
            }
        }

        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    });
}

criterion_group!(benches, fcu_unsafe_benches);
criterion_main!(benches);
