//! Benchmarks for state-prefetch worker throughput.

use alloy_primitives::{Address, U256};
use base_execution_state_prefetch::StatePrefetchPool;
use base_precompile_storage::{PrefetchRequest, StatePrefetcher};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use reth_provider::test_utils::MockEthProvider;

const REQUESTS: usize = 4_096;
const WORKERS: usize = 4;

fn state_prefetch_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("state_prefetch_pool");
    group.throughput(Throughput::Elements(REQUESTS as u64));
    group.bench_function("read_slots", |b| {
        b.iter_batched(
            || {
                let pool = StatePrefetchPool::spawn(MockEthProvider::default(), WORKERS);
                let address = Address::repeat_byte(0x01);
                let requests = (0..REQUESTS)
                    .map(|slot| PrefetchRequest { address, slot: U256::from(slot) })
                    .collect::<Vec<_>>();
                (pool, requests)
            },
            |(pool, requests)| {
                pool.prefetch(&requests);
                pool.join();
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, state_prefetch_pool);
criterion_main!(benches);
