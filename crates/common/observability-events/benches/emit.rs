//! Producer-thread cost of emitting one builder-shaped transaction event.
//!
//! The stage benchmarks split emission into the work the hot path performs today: converting
//! typed event data into a JSON map, computing the SHA-256 event ID, building the envelope,
//! validating it, and serializing it. `emit/file` runs the full path on the calling thread into a
//! real JSONL writer in a tight loop. `emit/file_chunked` runs the same path in timed chunks with
//! untimed pauses so writer queues drain, and `emit/file_deferred` uses those chunks to measure
//! only the calling-thread share when the event is built on the writer's render thread. Compare
//! the two chunked cases with each other; chunks start cold, so they read higher than the tight
//! loop. The bench fails if the writer dropped anything, because drops are cheaper than writes. `reference/tracing_json` formats an ordinary
//! `tracing` JSON log line with the same fields into the same kind of non-blocking writer, as a
//! baseline for normal application logging.

use std::{
    hint::black_box,
    io, thread,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, TxHash};
use base_observability_events::{
    DEFAULT_QUEUE_CAPACITY, EventIdBuilder, TransactionEvent, TransactionEventBuilder,
    TransactionEventProducer, TransactionEventType, TransactionEventWriter,
    TransactionEventWriterConfig,
};
use criterion::{Criterion, criterion_group, criterion_main};
use serde::Serialize;
use tracing_subscriber::{fmt, layer::SubscriberExt};

const NETWORK: &str = "bench";
const PAYLOAD_ID: &str = "0x03a1b2c3d4e5f607";

/// Mirrors the fields of a builder deferral event: shared context plus budget fields.
#[derive(Serialize)]
struct DeferredEventData {
    parent_hash: String,
    builder_mode: &'static str,
    source_queue: &'static str,
    target_flashblock_count: u64,
    flashblock_index: u64,
    ordering_position: u64,
    reason: &'static str,
    cumulative_gas_used: u64,
    cumulative_da_bytes_used: u64,
    cumulative_uncompressed_bytes: u64,
    block_gas_limit: u64,
    tx_data_limit: u64,
    block_data_limit: u64,
    tx_gas_limit: u64,
    tx_da_size: u64,
}

impl DeferredEventData {
    fn sample() -> Self {
        Self {
            parent_hash: format!("{:#x}", B256::repeat_byte(0x22)),
            builder_mode: "flashblocks",
            source_queue: "txpool_best",
            target_flashblock_count: 10,
            flashblock_index: 4,
            ordering_position: 1_234,
            reason: "validity_predicate_pending",
            cumulative_gas_used: 18_000_000,
            cumulative_da_bytes_used: 120_000,
            cumulative_uncompressed_bytes: 410_000,
            block_gas_limit: 150_000_000,
            tx_data_limit: 131_072,
            block_data_limit: 1_200_000,
            tx_gas_limit: 210_000,
            tx_da_size: 180,
        }
    }
}

fn deferred_event(builder: TransactionEventBuilder) -> TransactionEventBuilder {
    deferred_event_with(builder, DeferredEventData::sample())
}

fn deferred_event_with(
    builder: TransactionEventBuilder,
    data: DeferredEventData,
) -> TransactionEventBuilder {
    builder
        .tx_hash(TxHash::repeat_byte(0x11))
        .block_number(36_000_000)
        .payload_id(PAYLOAD_ID)
        .id_part("flashblock_index", 4)
        .id_part("ordering_position", 1_234)
        .typed_data(data)
}

fn new_builder() -> TransactionEventBuilder {
    TransactionEventBuilder::new(
        TransactionEventProducer::BaseBuilder,
        TransactionEventType::BuilderDeferred,
    )
}

fn built_event() -> TransactionEvent {
    deferred_event(new_builder()).build_with_network(NETWORK)
}

fn stages(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage");
    group.bench_function("typed_data_to_map", |b| {
        b.iter_batched(
            new_builder,
            |builder| black_box(builder.typed_data(DeferredEventData::sample())),
            criterion::BatchSize::SmallInput,
        )
    });
    group.bench_function("event_id", |b| {
        b.iter(|| {
            EventIdBuilder::new()
                .part("producer", TransactionEventProducer::BaseBuilder)
                .part("event_type", TransactionEventType::BuilderDeferred)
                .part("tx_hash", black_box(TxHash::repeat_byte(0x11)))
                .part("block_number", black_box(36_000_000_u64))
                .part("payload_id", black_box(PAYLOAD_ID))
                .part("flashblock_index", black_box(4_u64))
                .part("ordering_position", black_box(1_234_u64))
                .finish()
        })
    });
    group.bench_function("build_envelope", |b| {
        b.iter_batched(
            || deferred_event(new_builder()),
            |builder| black_box(builder.build_with_network(NETWORK)),
            criterion::BatchSize::SmallInput,
        )
    });
    let event = built_event();
    group.bench_function("validate", |b| b.iter(|| black_box(&event).validate()));
    group.bench_function("serialize", |b| {
        b.iter(|| serde_json::to_vec(black_box(&event)).expect("event serializes"))
    });
    group.finish();
}

/// Events emitted per timed chunk. Small enough that neither bounded queue fills.
const CHUNK: u64 = 1_024;
/// Untimed pause after each chunk so the writer threads drain before the next one.
const DRAIN_PAUSE: Duration = Duration::from_millis(10);

fn file_writer(dir: &tempfile::TempDir, name: &str) -> TransactionEventWriter {
    TransactionEventWriter::from_config(TransactionEventWriterConfig {
        enabled: true,
        file_path: dir.path().join(format!("{name}.jsonl")),
        queue_capacity: DEFAULT_QUEUE_CAPACITY,
        max_file_bytes: 64 * 1024 * 1024,
        max_files: 2,
        required: true,
        producer: TransactionEventProducer::BaseBuilder,
        network: NETWORK.to_string(),
    })
    .expect("file writer")
}

/// Times `iters` calls of `emit` in drainable chunks, excluding the pauses.
fn time_in_chunks(iters: u64, mut emit: impl FnMut()) -> Duration {
    let mut elapsed = Duration::ZERO;
    let mut remaining = iters;
    while remaining > 0 {
        let chunk = remaining.min(CHUNK);
        let start = Instant::now();
        for _ in 0..chunk {
            emit();
        }
        elapsed += start.elapsed();
        remaining -= chunk;
        thread::sleep(DRAIN_PAUSE);
    }
    elapsed
}

/// Fails the bench if `writer` dropped events, because drops are cheaper than writes.
fn assert_no_drops(case: &str, writer: &TransactionEventWriter) {
    let dropped = writer.dropped_events();
    eprintln!("{case}: writer dropped {dropped} events");
    assert_eq!(dropped, 0, "{case}: drops make emit timings look cheaper than they are");
}

fn emit(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut group = c.benchmark_group("emit");

    group.bench_function("disabled", |b| {
        b.iter(|| {
            TransactionEventBuilder::emit_with(
                None,
                TransactionEventProducer::BaseBuilder,
                TransactionEventType::BuilderDeferred,
                deferred_event,
            )
        })
    });

    let writer = file_writer(&dir, "file");
    group.bench_function("file", |b| {
        b.iter(|| {
            TransactionEventBuilder::emit_with(
                Some(&writer),
                TransactionEventProducer::BaseBuilder,
                TransactionEventType::BuilderDeferred,
                deferred_event,
            )
        })
    });
    assert_no_drops("emit/file", &writer);

    let writer = file_writer(&dir, "file_chunked");
    group.bench_function("file_chunked", |b| {
        b.iter_custom(|iters| {
            time_in_chunks(iters, || {
                let _ = black_box(TransactionEventBuilder::emit_with(
                    Some(&writer),
                    TransactionEventProducer::BaseBuilder,
                    TransactionEventType::BuilderDeferred,
                    deferred_event,
                ));
            })
        })
    });
    assert_no_drops("emit/file_chunked", &writer);

    let writer = file_writer(&dir, "file_deferred");
    group.bench_function("file_deferred", |b| {
        b.iter_custom(|iters| {
            time_in_chunks(iters, || {
                let _ = black_box(TransactionEventBuilder::emit_deferred(
                    Some(&writer),
                    TransactionEventProducer::BaseBuilder,
                    TransactionEventType::BuilderDeferred,
                    DeferredEventData::sample,
                    deferred_event_with,
                ));
            })
        })
    });
    assert_no_drops("emit/file_deferred", &writer);

    group.finish();
}

fn reference(c: &mut Criterion) {
    let (sink, _guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(DEFAULT_QUEUE_CAPACITY)
        .finish(io::sink());
    let subscriber = tracing_subscriber::registry().with(fmt::layer().json().with_writer(sink));
    let data = DeferredEventData::sample();
    let tx_hash = TxHash::repeat_byte(0x11);

    let mut group = c.benchmark_group("reference");
    tracing::subscriber::with_default(subscriber, || {
        group.bench_function("tracing_json", |b| {
            b.iter(|| {
                tracing::info!(
                    tx_hash = %tx_hash,
                    block_number = 36_000_000_u64,
                    payload_id = PAYLOAD_ID,
                    parent_hash = %data.parent_hash,
                    builder_mode = data.builder_mode,
                    source_queue = data.source_queue,
                    target_flashblock_count = data.target_flashblock_count,
                    flashblock_index = data.flashblock_index,
                    ordering_position = data.ordering_position,
                    reason = data.reason,
                    cumulative_gas_used = data.cumulative_gas_used,
                    cumulative_da_bytes_used = data.cumulative_da_bytes_used,
                    cumulative_uncompressed_bytes = data.cumulative_uncompressed_bytes,
                    block_gas_limit = data.block_gas_limit,
                    tx_data_limit = data.tx_data_limit,
                    block_data_limit = data.block_data_limit,
                    tx_gas_limit = data.tx_gas_limit,
                    tx_da_size = data.tx_da_size,
                    "builder deferred"
                )
            })
        });
    });
    group.finish();
}

criterion_group!(benches, stages, emit, reference);
criterion_main!(benches);
