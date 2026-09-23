//! Throughput benchmarks for incompressible and deliberately compressible channel inputs.
//!
//! Use `base-compression-benchmark` for the corresponding compressed-byte report. Criterion
//! measures wall-clock compression throughput here; it deliberately does not conflate time with
//! the resulting channel size.

use std::hint::black_box;

use base_comp::{CompressionBenchmark, CompressionScenario, InputPattern, TransactionProfile};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const PROFILES: [TransactionProfile; 3] = [
    TransactionProfile { name: "Native ETH transfer", encoded_bytes: 100 },
    TransactionProfile { name: "ERC-4337 smart-wallet UserOp", encoded_bytes: 448 },
    TransactionProfile { name: "Contract deployment", encoded_bytes: 3261 },
];

const TRANSACTIONS_PER_BATCH: usize = 128;
const BATCHES_PER_CHANNEL: usize = 8;

fn bench_synthetic_channel_compression(c: &mut Criterion) {
    let benchmark = CompressionBenchmark;
    let mut group = c.benchmark_group("synthetic_channel_compression");

    for pattern in [InputPattern::Pseudorandom, InputPattern::Incrementing] {
        for profile in PROFILES {
            let scenario = CompressionScenario {
                profile,
                pattern,
                transactions_per_batch: TRANSACTIONS_PER_BATCH,
                batches_per_channel: BATCHES_PER_CHANNEL,
            };
            let uncompressed_bytes =
                profile.encoded_bytes.saturating_mul(scenario.transaction_count());
            group.throughput(Throughput::Bytes(uncompressed_bytes as u64));
            group.bench_with_input(
                BenchmarkId::new(pattern.label(), profile.name),
                &scenario,
                |bench, scenario| {
                    bench.iter(|| black_box(benchmark.measure(black_box(*scenario)).unwrap()));
                },
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_synthetic_channel_compression);
criterion_main!(benches);
