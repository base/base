//! Deterministic instruction-count benchmarks for `FastLZ` size estimation.
//!
//! This is the `iai-callgrind` counterpart to `flz.rs`. Instead of timing wall-clock
//! on a shared runner — where cache and scheduling noise produce bimodal, untrustworthy
//! deltas — it runs the same routines under Valgrind's Cachegrind simulator and counts
//! retired instructions (plus modeled cache traffic). The count is identical on every
//! run of the same code, so a base-vs-head delta reflects a code change and nothing else.
//!
//! `flz_compress_len` is CPU-bound with a small, cache-resident working set, so its
//! instruction count is a faithful proxy for real wall-clock time: fewer instructions
//! genuinely means faster on hardware. It is exactly the class of benchmark that belongs
//! in a deterministic gating signal.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::hint::black_box;

use base_common_flz::{data_gas_fjord, flz_compress_len, tx_estimated_size_fjord};
use hex_literal::hex;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

/// A representative encoded EIP-1559 contract-call transaction (~170 bytes).
const REAL_CONTRACT_CALL: &[u8] = &hex!(
    "02f901550a758302df1483be21b88304743f94f80e51afb613d764fa61751affd3313c190a86bb870151bd62fd12adb8e41ef24f3f000000000000000000000000000000000000000000000000000000000000006e000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e5831000000000000000000000000000000000000000000000000000000000003c1e5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000148c89ed219d02f1a5be012c689b4f5b731827bebe000000000000000000000000c001a033fd89cb37c31b2cba46b6466e040c61fc9b2a3675a7f5f493ebd5ad77c497f8a07cdf65680e238392693019b4092f610222e71b7cec06449cb922b93b6a12744e"
);

/// Build a deterministic calldata-like blob of the given length. Runs in the unmeasured
/// setup phase so allocation and generation are excluded from the counted region, exactly
/// as the Criterion version hoists it out of the timed closure.
fn synthetic_calldata(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 256) as u8).collect()
}

#[library_benchmark]
#[bench::real_contract_call(REAL_CONTRACT_CALL.to_vec())]
#[benches::synthetic(args = [128usize, 1024, 8192], setup = synthetic_calldata)]
fn compress_len(input: Vec<u8>) -> u32 {
    black_box(flz_compress_len(black_box(&input)))
}

#[library_benchmark]
fn tx_estimated_size() -> u64 {
    black_box(tx_estimated_size_fjord(black_box(REAL_CONTRACT_CALL)))
}

#[library_benchmark]
fn data_gas() -> u64 {
    black_box(data_gas_fjord(black_box(REAL_CONTRACT_CALL)))
}

library_benchmark_group!(
    name = flz;
    benchmarks = compress_len, tx_estimated_size, data_gas
);

main!(library_benchmark_groups = flz);
