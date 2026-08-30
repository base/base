//! Benchmarks for brotli channel decompression.
//!
//! Post-Fjord, brotli is the channel compression algorithm, so this runs once per
//! channel — roughly once per L1 block — for the whole of derivation. The dominant
//! cost is not the decompression itself but the scratch memory the decoder is handed,
//! so the benchmark deliberately measures the whole `decompress` call rather than an
//! isolated inner loop.
//!
//! Fixtures are real mainnet data: `channel_brotli.hex` is a brotli-compressed mainnet
//! channel, and the large arm re-compresses the decompressed contents of the
//! `batch.hex` zlib channel with the same quality and window size the batcher uses.

use std::hint::black_box;

use base_common_genesis::RollupConfig;
use base_protocol::{BatchReader, Brotli};
use brotli::enc::BrotliEncoderParams;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

/// Sliding-window exponent used by the batcher (`crates/batcher/comp/src/stream.rs`).
const BATCHER_LGWIN: i32 = 22;

/// Compression quality used by the batcher (`CompressionAlgo::Brotli10`).
const BATCHER_QUALITY: i32 = 10;

const MAX_RLP_BYTES: usize = RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize;

/// Decodes a hex fixture that may carry a trailing newline.
fn decode_hex_fixture(contents: &str) -> Vec<u8> {
    alloy_primitives::hex::decode(contents.trim_end()).expect("fixture is valid hex")
}

/// A real brotli-compressed mainnet channel, ~12 `KiB`.
fn small_channel() -> Vec<u8> {
    decode_hex_fixture(include_str!("../testdata/channel_brotli.hex"))
}

/// A large channel: the `batch.hex` zlib fixture's contents re-compressed with brotli
/// at the batcher's quality and window size.
fn large_channel() -> Vec<u8> {
    let zlib = decode_hex_fixture(include_str!("../testdata/batch.hex"));
    let mut reader = BatchReader::new(zlib, MAX_RLP_BYTES, false);
    // The fixture is zlib, so the brotli scratch is never touched here.
    reader.decompress(&mut Brotli::new()).expect("zlib fixture decompresses");

    let mut compressed = Vec::new();
    brotli::BrotliCompress(
        &mut reader.decompressed.as_slice(),
        &mut compressed,
        &BrotliEncoderParams {
            quality: BATCHER_QUALITY,
            lgwin: BATCHER_LGWIN,
            ..Default::default()
        },
    )
    .expect("brotli compression succeeds");
    compressed
}

fn bench_decompress(c: &mut Criterion) {
    let small = small_channel();
    let large = large_channel();

    // The decompressor is hoisted out of the timing loop because that is where it lives
    // in production: `ChannelReader` holds one for the life of the pipeline and feeds it
    // one channel after another.
    let mut brotli = Brotli::new();

    // Fail loudly at setup rather than reporting a timing for a broken fixture. This also
    // warms the scratch pools so the first sample is not an outlier.
    let small_out = brotli.decompress(&small, MAX_RLP_BYTES).expect("small channel decompresses");
    let large_out = brotli.decompress(&large, MAX_RLP_BYTES).expect("large channel decompresses");

    let mut group = c.benchmark_group("decompress");

    group.throughput(Throughput::Bytes(small_out.len() as u64));
    group.bench_function("channel/small", |b| {
        b.iter(|| black_box(brotli.decompress(black_box(&small), MAX_RLP_BYTES).unwrap()));
    });

    group.throughput(Throughput::Bytes(large_out.len() as u64));
    group.bench_function("channel/large", |b| {
        b.iter(|| black_box(brotli.decompress(black_box(&large), MAX_RLP_BYTES).unwrap()));
    });

    // Allocates the scratch pools on every call, which is what a per-channel decompressor
    // does. Kept as a guard: if the arms above ever converge on this one, scratch reuse has
    // regressed.
    group.throughput(Throughput::Bytes(small_out.len() as u64));
    group.bench_function("channel/small_cold_scratch", |b| {
        b.iter(|| black_box(Brotli::new().decompress(black_box(&small), MAX_RLP_BYTES).unwrap()));
    });

    group.finish();
}

criterion_group!(benches, bench_decompress);
criterion_main!(benches);
