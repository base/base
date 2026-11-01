//! Per-build cost of `best_transactions` on a large `BaseTransactionPool`.
//!
//! The builder creates one merged best-transactions iterator per flashblock (about 10 per block)
//! and per native block after Denim. Each arm measures that path with reth's pending and queued
//! subpools and the validity pool filled to the order of their configured limits:
//!
//! - `snapshot`: creating the iterator, which includes the cross-store lane bookkeeping the
//!   merge needs (validity lanes riding a standard run, standard transactions queued behind a
//!   validity run).
//! - `first_300`: the snapshot plus yielding a block's worth of candidates.
//! - `drain_shared`: the snapshot plus yielding every shared validity head and the standard
//!   transactions it releases, so the merge accumulates many released lanes at once.
//!
//! The `isolated` pool has no sender with transactions in two stores. `shared` adds 1 000
//! senders whose standard transaction is queued right behind their validity transaction, which
//! is the case the merge has to bridge; there the standard successor outbids everything else, so
//! it is taken right after its validity head. `shared_waiting` prices those successors below
//! everything else instead, so every released successor waits in the merge while the rest of
//! the pool is selected.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench best_transactions
//! ```

mod support;

use std::hint::black_box;

use base_execution_txpool::BasePooledTransaction;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_transaction_pool::{TransactionOrigin, TransactionPool};
use support::{BenchPool, build_pool, funded_signer, standard, validity};

/// Standard transactions pending in reth, one per sender.
const PENDING_STANDARD: usize = 10_000;
/// Standard transactions queued in reth behind a nonce gap; also the queued subpool limit.
const QUEUED_STANDARD: usize = 20_000;
/// Resting validity transactions, one per sender.
const VALIDITY: usize = 25_000;
/// Senders holding a validity transaction with a standard one queued behind it.
const SHARED_LANES: usize = 1_000;
/// Candidates a block's worth of selection pulls from the iterator.
const BLOCK_CANDIDATES: usize = 300;
/// Tip of the standard successor in `shared`: above every other transaction in the pool.
const OUTBIDDING_TIP: u128 = 60;
/// Tip of the standard successor in `shared_waiting`: below every other transaction.
const WAITING_TIP: u128 = 1;

async fn add(pool: &BenchPool, transactions: Vec<BasePooledTransaction>) {
    for transaction in transactions {
        pool.add_transaction(TransactionOrigin::External, transaction).await.unwrap();
    }
}

/// A pool filled to the sizes above. `shared_lanes` senders hold both a resting validity
/// transaction at nonce 0 and a standard transaction at nonce 1 paying `successor_tip`, which
/// reth queues behind the gap.
///
/// The runtime hosting the validation tasks is returned with the pool so it outlives the
/// measurement.
fn large_pool(shared_lanes: usize, successor_tip: u128) -> (BenchPool, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let pool = runtime.block_on(async {
        let (pool, client) = build_pool(QUEUED_STANDARD);
        let pending: Vec<_> = (0..PENDING_STANDARD)
            .map(|index| standard(&funded_signer(&client), 0, 2 + index as u128 % 50))
            .collect();
        add(&pool, pending).await;
        let queued: Vec<_> = (0..QUEUED_STANDARD - shared_lanes)
            .map(|index| standard(&funded_signer(&client), 1, 2 + index as u128 % 50))
            .collect();
        add(&pool, queued).await;
        let resting: Vec<_> = (0..VALIDITY - shared_lanes)
            .map(|index| validity(&funded_signer(&client), 0, 2 + index as u128 % 50))
            .collect();
        add(&pool, resting).await;
        let mut shared = Vec::with_capacity(shared_lanes * 2);
        for index in 0..shared_lanes {
            let signer = funded_signer(&client);
            shared.push(validity(&signer, 0, 2 + index as u128 % 50));
            shared.push(standard(&signer, 1, successor_tip));
        }
        add(&pool, shared).await;
        pool
    });
    let size = pool.pool_size();
    assert_eq!(size.pending, PENDING_STANDARD + VALIDITY, "validity transactions are pending");
    assert_eq!(size.queued, QUEUED_STANDARD, "standard successors are queued");
    (pool, runtime)
}

fn best_transactions_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("txpool/best_transactions");
    group.sample_size(20);
    for (name, shared_lanes, successor_tip) in [
        ("isolated", 0, OUTBIDDING_TIP),
        ("shared", SHARED_LANES, OUTBIDDING_TIP),
        ("shared_waiting", SHARED_LANES, WAITING_TIP),
    ] {
        let (pool, _runtime) = large_pool(shared_lanes, successor_tip);
        group.bench_with_input(BenchmarkId::new("snapshot", name), &pool, |b, pool| {
            b.iter(|| black_box(pool.best_transactions()));
        });
        group.bench_with_input(BenchmarkId::new("first_300", name), &pool, |b, pool| {
            b.iter(|| {
                let best = pool.best_transactions();
                assert_eq!(best.take(BLOCK_CANDIDATES).map(black_box).count(), BLOCK_CANDIDATES);
            });
        });
        if shared_lanes == 0 {
            continue;
        }
        // Every pending transaction plus the shared successors; the whole pool
        // is drained, so each released successor is yielded wherever its tip
        // ranks it.
        let drained = PENDING_STANDARD + VALIDITY + shared_lanes;
        group.bench_with_input(BenchmarkId::new("drain_shared", name), &pool, |b, pool| {
            b.iter(|| {
                let best = pool.best_transactions();
                assert_eq!(best.map(black_box).count(), drained);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, best_transactions_benches);
criterion_main!(benches);
