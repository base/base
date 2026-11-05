//! Cost of the rejection-cache lookup both builders make for every candidate transaction.
//!
//! Each arm looks up one flashblock's worth of candidates, [`CANDIDATES_PER_BUILD`], against a
//! cache holding `entries` hashes. Most candidates were never rejected, so most lookups miss: the
//! `miss` arm is the common case, `hit` is the skip of a recently rejected transaction.
//!
//! ```sh
//! cargo bench -p base-execution-payload-builder --bench rejection_cache
//! ```

use std::hint::black_box;

use alloy_primitives::{B256, TxHash};
use base_execution_payload_builder::{REJECTION_CACHE_TTL, RejectionCache};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// Candidates one large flashblock build considers.
const CANDIDATES_PER_BUILD: usize = 25_000;

/// Cache sizes, from a small cache up to a large one.
const ENTRIES: [usize; 3] = [1_000, 10_000, 100_000];

/// Distinct, uniformly distributed hashes, as transaction hashes are.
fn hashes(seed: u64, count: usize) -> Vec<TxHash> {
    (0..count as u64)
        .map(|index| alloy_primitives::keccak256([seed, index].map(u64::to_le_bytes).concat()))
        .collect()
}

fn rejection_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("rejection_cache");
    group.throughput(Throughput::Elements(CANDIDATES_PER_BUILD as u64));
    for entries in ENTRIES {
        let cache = RejectionCache::new(entries as u64, REJECTION_CACHE_TTL);
        let rejected = hashes(1, entries);
        cache.mark_rejected(&rejected);
        cache.run_pending_tasks();

        let misses = hashes(2, CANDIDATES_PER_BUILD);
        let hits: Vec<B256> = rejected.iter().copied().cycle().take(CANDIDATES_PER_BUILD).collect();
        for (name, candidates) in [("miss", &misses), ("hit", &hits)] {
            group.bench_with_input(
                BenchmarkId::new(name, format!("entries={entries}")),
                candidates,
                |b, candidates| {
                    b.iter(|| {
                        candidates.iter().filter(|hash| cache.is_rejected(black_box(hash))).count()
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, rejection_cache);
criterion_main!(benches);
