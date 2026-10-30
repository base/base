//! Cost of admitting a gossip or RPC batch into `BaseTransactionPool`.
//!
//! Transaction admission runs for every gossiped and submitted transaction (see
//! `docs/guides/HOT_PATH_PERFORMANCE_REVIEW.md`), and reth hands the pool whole batches. Standard
//! transactions in a batch are validated together against one state snapshot; transactions
//! routed to a lane store (validity or sidecar) are admitted one by one. Each arm admits a batch
//! of fresh senders into an empty pool:
//!
//! - `standard`: every transaction is a standard EIP-1559 transaction.
//! - `mixed`: the same batch with one validity transaction in it, which must not push the
//!   standard transactions off the batched path.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench admission
//! ```

mod support;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_transaction_pool::{TransactionOrigin, TransactionPool};
use support::{build_pool, funded_signer, standard, validity};

/// Transactions per admitted batch.
const BATCH: usize = 256;
/// Queued subpool limit of the bench pool; nothing is queued here.
const QUEUED_LIMIT: usize = 1_000;
/// Tip of every transaction in the batch.
const TIP: u128 = 2;

fn admission_benches(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let mut group = c.benchmark_group("txpool/admission/batch");
    group.sample_size(100);
    for (arm, validity_count) in [("standard", 0), ("mixed", 1)] {
        group.bench_function(BenchmarkId::from_parameter(arm), |b| {
            b.iter_batched(
                || {
                    let _guard = runtime.enter();
                    let (pool, client) = build_pool(QUEUED_LIMIT);
                    let batch: Vec<_> = (0..BATCH)
                        .map(|index| {
                            let signer = funded_signer(&client);
                            if index < validity_count {
                                validity(&signer, 0, TIP)
                            } else {
                                standard(&signer, 0, TIP)
                            }
                        })
                        .collect();
                    (pool, batch)
                },
                |(pool, batch)| {
                    let results =
                        runtime.block_on(pool.add_transactions(TransactionOrigin::External, batch));
                    assert!(results.iter().all(Result::is_ok));
                    pool
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, admission_benches);
criterion_main!(benches);
