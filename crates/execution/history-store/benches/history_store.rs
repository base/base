//! Benchmark scaffold for the shared chain history store.
//!
//! Measures segment building from fixture blocks, and archived block reads on a cold reader
//! (segment fetched from the in-memory object store) and a warm reader (segment cached).
//! Object-store latency is not modelled; run against `MinIO` or S3 for that.

use std::{hint::black_box, sync::Arc};

use base_history_store::{
    ArchiveLayout, ArchiveReader, CacheConfig, DEFAULT_SNAPSHOT_TTL, ReaderConfig, SegmentBuilder,
    test_utils::{ChainFixture, FIXTURE_CHAIN_ID, fixture_writer, test_index_options},
};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use object_store::{ObjectStore, memory::InMemory};
use tokio::runtime::Runtime;

/// Blocks per segment in the benchmarks.
const N: u64 = 64;
/// Transactions per fixture block.
const TXS_PER_BLOCK: u64 = 4;

/// Opens a reader over a store holding one published segment.
fn published_reader(rt: &Runtime, fixture: &ChainFixture) -> ArchiveReader {
    rt.block_on(async {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let writer = fixture_writer(Arc::clone(&store), fixture, N).await;
        writer.step().await.expect("publish segment 0");
        writer.index.close().await.expect("close writer index");
        ArchiveReader::open(
            store,
            ArchiveLayout::new("chain"),
            ReaderConfig {
                chain_id: FIXTURE_CHAIN_ID,
                genesis_hash: fixture.blocks[0].hash,
                chain_spec: Arc::clone(&fixture.chain_spec),
                cache: CacheConfig::default(),
                index: test_index_options(),
                snapshot_ttl: DEFAULT_SNAPSHOT_TTL,
            },
        )
        .await
        .expect("reader opens")
    })
}

/// Segment build and block read benchmarks.
fn history_store(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let fixture = ChainFixture::new(N, TXS_PER_BLOCK);

    let mut group = c.benchmark_group("history_store");
    group.throughput(Throughput::Elements(N));
    group.bench_function("segment_build", |b| {
        let builder = SegmentBuilder::new(N);
        b.iter(|| black_box(builder.build(0, black_box(&fixture.blocks)).expect("build")));
    });
    group.finish();

    let mut group = c.benchmark_group("history_store_read");
    group.throughput(Throughput::Elements(1));
    group.bench_function("block_cold", |b| {
        b.iter_batched(
            || published_reader(&rt, &fixture),
            |reader| {
                rt.block_on(async {
                    let snapshot = reader.snapshot().await.expect("snapshot");
                    black_box(reader.block(&snapshot, N / 2).await.expect("block"))
                })
            },
            criterion::BatchSize::PerIteration,
        );
    });
    let reader = published_reader(&rt, &fixture);
    group.bench_function("block_warm", |b| {
        b.iter(|| {
            rt.block_on(async {
                let snapshot = reader.snapshot().await.expect("snapshot");
                black_box(reader.block(&snapshot, N / 2).await.expect("block"))
            })
        });
    });
    group.finish();
}

criterion_group!(benches, history_store);
criterion_main!(benches);
