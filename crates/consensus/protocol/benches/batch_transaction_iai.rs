//! Deterministic instruction-count benchmarks for batch-transaction frame encoding.
//!
//! The `iai-callgrind` counterpart to `batch_transaction.rs`. Encoding is deterministic,
//! single-threaded, and CPU-bound; the `4 MiB` buffer is written sequentially (bandwidth,
//! not random-access latency), so instruction count tracks wall-clock well. Building the
//! batch runs in the unmeasured setup phase.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use alloy_primitives::Bytes;
use base_protocol::{BatchTransaction, Frame};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

const FRAME_COUNT: usize = 32;
const FRAME_DATA_LEN: usize = 128 * 1024;

/// Build the representative multi-frame batch. Runs in setup only.
fn make_batch() -> BatchTransaction {
    let frame = Frame::new([0xFF; 16], 0, vec![0xDD; FRAME_DATA_LEN], false);
    BatchTransaction {
        frames: vec![frame; FRAME_COUNT],
        size: FRAME_COUNT * (FRAME_DATA_LEN + Frame::ENCODED_OVERHEAD),
    }
}

/// Encode each frame into its own temporary buffer, then concatenate — the allocation-
/// heavier strategy the wall-clock bench contrasts against the in-place path.
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

#[library_benchmark]
#[bench::temporary_frame_buffers(make_batch())]
fn temporary_frame_buffers(batch: BatchTransaction) {
    black_box(encode_with_temporary_buffers(black_box(&batch)));
}

#[library_benchmark]
#[bench::encode_in_place(make_batch())]
fn encode_in_place(batch: BatchTransaction) {
    black_box(black_box(&batch).to_bytes());
}

library_benchmark_group!(
    name = batch_transaction;
    benchmarks = temporary_frame_buffers, encode_in_place
);

main!(library_benchmark_groups = batch_transaction);
