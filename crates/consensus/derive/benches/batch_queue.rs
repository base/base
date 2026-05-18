//! Benchmarks for [`BatchQueue`] span-batch cache handling.

use std::{collections::VecDeque, hint::black_box, sync::Arc};

use base_common_genesis::RollupConfig;
use base_consensus_derive::{
    BatchQueue,
    test_utils::{TestL2ChainProvider, TestNextBatchProvider},
};
use base_protocol::{L2BlockInfo, SingleBatch};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

const CACHED_SPANS: usize = 4_096;

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

fn drain_cached_spans(mut batch_queue: BatchQueue<TestNextBatchProvider, TestL2ChainProvider>) {
    let parent = L2BlockInfo::default();
    while !batch_queue.next_spans.is_empty() {
        black_box(batch_queue.pop_next_batch(parent).expect("cached span batch"));
    }
}

fn bench_batch_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_queue");
    group.throughput(Throughput::Elements(CACHED_SPANS as u64));
    group.bench_function("drain_cached_span_batches", |b| {
        b.iter_batched(
            || batch_queue_with_cached_spans(CACHED_SPANS),
            drain_cached_spans,
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_batch_queue);
criterion_main!(benches);
