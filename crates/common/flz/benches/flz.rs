//! Benchmarks for `FastLZ` compressed-size estimation.
//!
//! `flz_compress_len` runs once per transaction to price L1 data gas, so it sits on
//! the hot path of block building, the txpool, and receipt construction. It is a
//! deterministic, pure-CPU hashtable loop, which makes it a good per-PR advisory
//! signal.

use std::hint::black_box;

use base_common_flz::{data_gas_fjord, flz_compress_len, tx_estimated_size_fjord};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use hex_literal::hex;

/// A representative encoded EIP-1559 contract-call transaction (~170 bytes).
const REAL_CONTRACT_CALL: &[u8] = &hex!(
    "02f901550a758302df1483be21b88304743f94f80e51afb613d764fa61751affd3313c190a86bb870151bd62fd12adb8e41ef24f3f000000000000000000000000000000000000000000000000000000000000006e000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e5831000000000000000000000000000000000000000000000000000000000003c1e5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000148c89ed219d02f1a5be012c689b4f5b731827bebe000000000000000000000000c001a033fd89cb37c31b2cba46b6466e040c61fc9b2a3675a7f5f493ebd5ad77c497f8a07cdf65680e238392693019b4092f610222e71b7cec06449cb922b93b6a12744e"
);

/// Build a deterministic calldata-like blob of the given length. The repeating word
/// pattern gives `FastLZ` realistic back-references without depending on an RNG.
fn synthetic_calldata(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 256) as u8).collect()
}

fn bench_flz(c: &mut Criterion) {
    let mut inputs: Vec<(String, Vec<u8>)> =
        vec![("real_contract_call".to_string(), REAL_CONTRACT_CALL.to_vec())];
    for len in [128usize, 1024, 8192] {
        inputs.push((format!("synthetic_{len}"), synthetic_calldata(len)));
    }

    let mut group = c.benchmark_group("flz_compress_len");
    for (name, input) in &inputs {
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), input, |b, input| {
            b.iter(|| black_box(flz_compress_len(black_box(input))));
        });
    }
    group.finish();

    // The full Fjord data-gas pricing path (size estimate + gas), dominated by the
    // FastLZ pass but including the scaling arithmetic applied per transaction.
    let mut group = c.benchmark_group("fjord_pricing");
    group.bench_function("tx_estimated_size_fjord", |b| {
        b.iter(|| black_box(tx_estimated_size_fjord(black_box(REAL_CONTRACT_CALL))));
    });
    group.bench_function("data_gas_fjord", |b| {
        b.iter(|| black_box(data_gas_fjord(black_box(REAL_CONTRACT_CALL))));
    });
    group.finish();
}

criterion_group!(benches, bench_flz);
criterion_main!(benches);
