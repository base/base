//! Deterministic instruction-count benchmark for [`BatchQueue`] span-batch draining.
//!
//! The `iai-callgrind` counterpart to `batch_queue.rs`. Draining the cached span-batch
//! deque is deterministic and single-threaded. The wall-clock version uses
//! `iter_batched` to rebuild the deque outside the timed region; here that rebuild runs
//! in iai's unmeasured setup phase (the benchmark argument is evaluated before the
//! counted region), so only the drain is counted.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::{collections::VecDeque, hint::black_box, sync::Arc};

use base_common_genesis::RollupConfig;
use base_consensus_derive::{
    BatchQueue,
    test_utils::{TestL2ChainProvider, TestNextBatchProvider},
};
use base_protocol::{L2BlockInfo, SingleBatch};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

const CACHED_SPANS: usize = 4_096;

/// Build a batch queue pre-loaded with `len` cached span batches. Runs in setup only.
fn batch_queue_with_cached_spans(
    len: usize,
) -> BatchQueue<TestNextBatchProvider, TestL2ChainProvider> {
    let cfg = Arc::new(RollupConfig::default());
    let mock = TestNextBatchProvider::new(Vec::new());
    let fetcher = TestL2ChainProvider::default();
    let mut batch_queue = BatchQueue::new(cfg, mock, fetcher);
    batch_queue.next_spans = (0..len)
        .map(|i| SingleBatch { timestamp: i as u64, ..Default::default() })
        .collect::<VecDeque<_>>();
    batch_queue
}

/// Drain every cached span batch — the measured work.
fn drain_cached_spans(mut batch_queue: BatchQueue<TestNextBatchProvider, TestL2ChainProvider>) {
    let parent = L2BlockInfo::default();
    while !batch_queue.next_spans.is_empty() {
        black_box(batch_queue.pop_next_batch(parent).expect("cached span batch"));
    }
}

#[library_benchmark]
#[bench::drain_cached_span_batches(batch_queue_with_cached_spans(CACHED_SPANS))]
fn drain(batch_queue: BatchQueue<TestNextBatchProvider, TestL2ChainProvider>) {
    drain_cached_spans(black_box(batch_queue));
}

library_benchmark_group!(
    name = batch_queue;
    benchmarks = drain
);

main!(library_benchmark_groups = batch_queue);
