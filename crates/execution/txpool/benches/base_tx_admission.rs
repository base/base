//! Cost of admitting a batch of Base transactions into `BaseTransactionPool`.
//!
//! Each transaction is a self-paid, EOA-signed Base transaction (`0x79`) from a fresh funded
//! sender. Arms:
//!
//! - `protocol`: `nonce_key = 0`, which rests in reth's protocol pool.
//! - `two_d`: `nonce_key = 1`, which rests in the 2D-nonce sidecar store.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench base_tx_admission
//! ```

#[allow(dead_code, reason = "shared with the other benches")]
mod support;

use alloy_primitives::{Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BasePooledTransaction as PooledEnvelope, TxBase, TxBaseSigned};
use base_execution_txpool::BasePooledTransaction;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use support::{build_pool, chain_id, funded_signer};

/// Transactions per admitted batch.
const BATCH: usize = 256;
/// Queued subpool limit of the bench pool; nothing is queued here.
const QUEUED_LIMIT: usize = 1_000;

fn transaction(signer: &PrivateKeySigner, nonce_key: U256) -> BasePooledTransaction {
    let tx = TxBase {
        chain_id: chain_id(),
        sender: None,
        nonce_key,
        nonce_sequence: 0,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: 2,
        max_fee_per_gas: 1_000,
        gas_limit: 100_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: None,
    };
    let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
    let signed = TxBaseSigned::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
    BasePooledTransaction::from_pooled(Recovered::new_unchecked(
        PooledEnvelope::Base(signed),
        signer.address(),
    ))
}

fn base_tx_benches(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let mut group = c.benchmark_group("txpool/base_tx");
    group.sample_size(50);
    for (arm, nonce_key) in [("protocol", U256::ZERO), ("two_d", U256::from(1))] {
        group.bench_function(BenchmarkId::new("admit", arm), |b| {
            b.iter_batched(
                || {
                    let _guard = runtime.enter();
                    let (pool, client) = build_pool(QUEUED_LIMIT);
                    let batch: Vec<_> = (0..BATCH)
                        .map(|_| transaction(&funded_signer(&client), nonce_key))
                        .collect();
                    (pool, batch)
                },
                |(pool, batch)| {
                    let results =
                        runtime.block_on(pool.add_transactions(TransactionOrigin::External, batch));
                    assert!(
                        results.iter().all(Result::is_ok),
                        "{:?}",
                        results.iter().find(|r| r.is_err())
                    );
                    pool
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(base_tx, base_tx_benches);
criterion_main!(base_tx);
