//! Benchmarks for batch transaction frame encoding.

use std::hint::black_box;

use alloy_primitives::Bytes;
use base_protocol::{BatchTransaction, Frame};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const FRAME_COUNT: usize = 32;
const FRAME_DATA_LEN: usize = 128 * 1024;

fn encode_with_temporary_buffers(batch: &BatchTransaction) -> Bytes {
    batch
        .frames
        .iter()
        .fold(Vec::new(), |mut encoded, frame| {
            encoded.append(&mut frame.encode());
            encoded
        })
        .into()
}

fn bench_batch_transaction_encoding(c: &mut Criterion) {
    let frame = Frame::new([0xFF; 16], 0, vec![0xDD; FRAME_DATA_LEN], false);
    let batch = BatchTransaction {
        frames: vec![frame; FRAME_COUNT],
        size: FRAME_COUNT * (FRAME_DATA_LEN + Frame::ENCODED_OVERHEAD),
    };

    let mut group = c.benchmark_group("batch_transaction_encoding");
    group.throughput(Throughput::Bytes(batch.size() as u64));
    group.bench_function("temporary_frame_buffers", |b| {
        b.iter(|| black_box(encode_with_temporary_buffers(black_box(&batch))));
    });
    group.bench_function("encode_in_place", |b| {
        b.iter(|| black_box(black_box(&batch).to_bytes()));
    });
    group.finish();
}

criterion_group!(benches, bench_batch_transaction_encoding);
criterion_main!(benches);
