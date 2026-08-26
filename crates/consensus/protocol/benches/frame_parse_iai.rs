//! Deterministic instruction-count benchmarks for channel-frame parsing.
//!
//! The `iai-callgrind` counterpart to `frame_parse.rs`. `Frame::parse_frames` /
//! `Frame::decode` are deterministic, single-threaded, pure-CPU decode routines, so
//! their retired instruction count is a faithful, zero-variance proxy for wall-clock
//! time. Encoding the input runs in the unmeasured setup phase so only the parse work
//! is counted.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use base_protocol::{DERIVATION_VERSION_0, Frame};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

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

/// A single `4 KiB` frame encoded on its own for the `decode` path.
fn build_single() -> Vec<u8> {
    let mut single = Vec::new();
    Frame::new([0xAB; 16], 0, vec![0xCD; 4096], true).encode_into(&mut single);
    single
}

#[library_benchmark]
#[bench::many_small(build_encoded(256, 1024))]
#[bench::few_large(build_encoded(8, 128 * 1024))]
fn parse_frames(encoded: Vec<u8>) {
    black_box(Frame::parse_frames(black_box(&encoded)).unwrap());
}

#[library_benchmark]
#[bench::single_4kib(build_single())]
fn decode(encoded: Vec<u8>) {
    black_box(Frame::decode(black_box(&encoded)).unwrap());
}

library_benchmark_group!(
    name = frame_parse;
    benchmarks = parse_frames, decode
);

main!(library_benchmark_groups = frame_parse);
