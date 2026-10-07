//! Producer-thread cost of emitting one builder-shaped transaction event.
//!
//! The stage benchmarks split emission into the work the hot path performs today: converting
//! typed event data into a JSON map, building the envelope (including the SHA-256 event ID),
//! validating it, and serializing it. `emit/file` runs the full path into a real JSONL writer.
//! When its bounded queue is full the writer drops instead of blocking, so that case measures
//! the caller-side cost including any drops. `reference/tracing_json` formats an ordinary
//! `tracing` JSON log line with the same fields into the same kind of non-blocking writer, as a
//! baseline for normal application logging.

use std::{hint::black_box, io};

use alloy_primitives::{B256, TxHash};
use base_observability_events::{
    DEFAULT_QUEUE_CAPACITY, TransactionEvent, TransactionEventBuilder, TransactionEventProducer,
    TransactionEventType, TransactionEventWriter, TransactionEventWriterConfig,
};
use criterion::{Criterion, criterion_group, criterion_main};
use serde::Serialize;
use serde_json::{Map, Value};
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

fn data_map() -> Map<String, Value> {
    serde_json::to_value(DeferredEventData::sample())
        .expect("bench data serializes")
        .as_object()
        .expect("bench data is an object")
        .clone()
}

fn deferred_event(
    builder: TransactionEventBuilder,
    data: Map<String, Value>,
) -> TransactionEventBuilder {
    builder
        .tx_hash(TxHash::repeat_byte(0x11))
        .block_number(36_000_000)
        .payload_id(PAYLOAD_ID)
        .id_part("flashblock_index", 4)
        .id_part("ordering_position", 1_234)
        .data(data)
}

fn new_builder() -> TransactionEventBuilder {
    TransactionEventBuilder::new(
        TransactionEventProducer::BaseBuilder,
        TransactionEventType::BuilderDeferred,
    )
}

fn built_event() -> TransactionEvent {
    deferred_event(new_builder(), data_map()).build_with_network(NETWORK)
}

fn stages(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage");
    group.bench_function("typed_data_to_map", |b| b.iter(|| black_box(data_map())));
    group.bench_function("build_envelope", |b| {
        b.iter_batched(
            data_map,
            |data| black_box(deferred_event(new_builder(), data).build_with_network(NETWORK)),
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

fn emit(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let writer = TransactionEventWriter::from_config(TransactionEventWriterConfig {
        enabled: true,
        file_path: dir.path().join("transaction-events.jsonl"),
        queue_capacity: DEFAULT_QUEUE_CAPACITY,
        max_file_bytes: 64 * 1024 * 1024,
        max_files: 2,
        required: true,
        producer: TransactionEventProducer::BaseBuilder,
        network: NETWORK.to_string(),
    })
    .expect("file writer");

    let mut group = c.benchmark_group("emit");
    group.bench_function("disabled", |b| {
        b.iter(|| {
            TransactionEventBuilder::emit_with(
                None,
                TransactionEventProducer::BaseBuilder,
                TransactionEventType::BuilderDeferred,
                |builder| deferred_event(builder, data_map()),
            )
        })
    });
    group.bench_function("file", |b| {
        b.iter(|| {
            TransactionEventBuilder::emit_with(
                Some(&writer),
                TransactionEventProducer::BaseBuilder,
                TransactionEventType::BuilderDeferred,
                |builder| deferred_event(builder, data_map()),
            )
        })
    });
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
