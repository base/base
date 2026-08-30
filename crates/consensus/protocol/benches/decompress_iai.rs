//! Deterministic instruction-count benchmarks for brotli channel decompression.
//!
//! The `iai-callgrind` counterpart to `decompress.rs`. The win being tracked here is
//! allocator and page-fault behaviour rather than decode arithmetic, and wall-clock
//! benchmarks measure that noisily on shared CI runners, so retired instruction counts
//! are the stable signal. Fixture decoding runs in the unmeasured setup phase so only
//! the decompression work is counted.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use base_common_genesis::RollupConfig;
use base_protocol::Brotli;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

const MAX_RLP_BYTES: usize = RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize;

/// A real brotli-compressed mainnet channel, ~12 `KiB`.
fn mainnet_channel() -> Vec<u8> {
    let contents = include_str!("../testdata/channel_brotli.hex");
    alloy_primitives::hex::decode(contents.trim_end()).expect("fixture is valid hex")
}

/// A decompressor whose scratch pools are already live, paired with a channel to feed it.
///
/// Warming happens here, in the unmeasured setup phase, so the measured call reflects
/// steady-state derivation rather than the one-off pool allocation.
fn warm_decompressor() -> (Brotli, Vec<u8>) {
    let channel = mainnet_channel();
    let mut brotli = Brotli::new();
    brotli.decompress(&channel, MAX_RLP_BYTES).expect("fixture decompresses");
    (brotli, channel)
}

#[library_benchmark]
#[bench::mainnet(warm_decompressor())]
fn decompress(input: (Brotli, Vec<u8>)) {
    let (mut brotli, channel) = input;
    black_box(brotli.decompress(black_box(&channel), MAX_RLP_BYTES).unwrap());
}

// Counterpart to `decompress` that pays the scratch allocation inside the measured
// region, so the two together attribute the cost to allocation rather than decode.
// `library_benchmark` rejects doc comments on the functions it wraps.
#[library_benchmark]
#[bench::mainnet(mainnet_channel())]
fn decompress_cold_scratch(channel: Vec<u8>) {
    black_box(Brotli::new().decompress(black_box(&channel), MAX_RLP_BYTES).unwrap());
}

library_benchmark_group!(
    name = decompress_group;
    benchmarks = decompress, decompress_cold_scratch
);

main!(library_benchmark_groups = decompress_group);
