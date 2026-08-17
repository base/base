//! Benchmarks for parsing channel frames out of L1 calldata.
//!
//! `Frame::parse_frames` is the decode counterpart to the `batch_transaction` encode
//! benchmark: it runs on every derivation pass to split calldata/blob payloads back
//! into frames. It is deterministic and pure-CPU, so it is a good per-PR advisory
//! signal.

use std::hint::black_box;

use base_protocol::{DERIVATION_VERSION_0, Frame};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

/// Encode `frame_count` frames of `data_len` bytes each into the on-chain
/// `DerivationVersion0 ++ Frame(s)` layout that `parse_frames` expects.
fn build_encoded(frame_count: usize, data_len: usize) -> Vec<u8> {
    let mut encoded = vec![DERIVATION_VERSION_0];
    for i in 0..frame_count {
        let is_last = i + 1 == frame_count;
        Frame::new([0xAB; 16], i as u16, vec![0xCD; data_len], is_last).encode_into(&mut encoded);
    }
    encoded
}

fn bench_frame_parse(c: &mut Criterion) {
    // Many small frames stresses per-frame overhead; a few large frames stresses the
    // bulk copy. Both shapes occur on the derivation path.
    let many_small = build_encoded(256, 1024);
    let few_large = build_encoded(8, 128 * 1024);

    let mut single = Vec::new();
    Frame::new([0xAB; 16], 0, vec![0xCD; 4096], true).encode_into(&mut single);

    // Fail loudly at setup if the encoding stops round-tripping.
    Frame::parse_frames(&many_small).expect("many_small parses");
    Frame::parse_frames(&few_large).expect("few_large parses");

    let mut group = c.benchmark_group("frame_parse");
    group.throughput(Throughput::Bytes(many_small.len() as u64));
    group.bench_function("parse_frames/256x1KiB", |b| {
        b.iter(|| black_box(Frame::parse_frames(black_box(&many_small)).unwrap()));
    });
    group.throughput(Throughput::Bytes(few_large.len() as u64));
    group.bench_function("parse_frames/8x128KiB", |b| {
        b.iter(|| black_box(Frame::parse_frames(black_box(&few_large)).unwrap()));
    });
    group.throughput(Throughput::Bytes(single.len() as u64));
    group.bench_function("decode/single_4KiB", |b| {
        b.iter(|| black_box(Frame::decode(black_box(&single)).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, bench_frame_parse);
criterion_main!(benches);
