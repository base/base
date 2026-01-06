#![allow(missing_docs)]

//! Benchmark for sender recovery performance.
//!
//! Compares sequential vs parallel ECDSA sender recovery
//! as requested in https://github.com/base/node-reth/issues/282

use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::{Address, B256};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rayon::prelude::*;
use reth_optimism_primitives::OpTransactionSigned;
use reth_transaction_pool::test_utils::TransactionBuilder;

/// Generate signed transactions from multiple unique signers.
fn generate_transactions(count: usize) -> Vec<OpTransactionSigned> {
    (0..count)
        .map(|i| {
            // Create unique signer for each transaction to simulate varying senders
            let mut signer_bytes = [0u8; 32];
            signer_bytes[0] = ((i + 1) % 256) as u8;
            signer_bytes[1] = ((i + 1) / 256) as u8;
            signer_bytes[31] = 1; // Ensure non-zero
            let signer = B256::from(signer_bytes);

            let txn = TransactionBuilder::default()
                .signer(signer)
                .chain_id(8453) // Base mainnet
                .to(Address::random())
                .nonce(0)
                .value(1_000_000_000u128)
                .gas_limit(21_000)
                .max_fee_per_gas(1_000_000_000)
                .max_priority_fee_per_gas(1_000_000_000)
                .into_eip1559()
                .as_eip1559()
                .expect("should convert to eip1559")
                .clone();

            OpTransactionSigned::Eip1559(txn)
        })
        .collect()
}

/// Sequential sender recovery (baseline)
fn recover_senders_sequential(txs: &[OpTransactionSigned]) -> Vec<Address> {
    txs.iter().map(|tx| tx.recover_signer().expect("valid signature")).collect()
}

/// Parallel sender recovery using rayon
fn recover_senders_parallel(txs: &[OpTransactionSigned]) -> Vec<Address> {
    txs.par_iter().map(|tx| tx.recover_signer().expect("valid signature")).collect()
}

fn sender_recovery_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender_recovery");

    // Test with 30, 40, and 100 transactions as requested in issue #282
    for tx_count in [30, 40, 100] {
        let transactions = generate_transactions(tx_count);

        group.bench_with_input(
            BenchmarkId::new("sequential", tx_count),
            &transactions,
            |b, txs| {
                b.iter(|| recover_senders_sequential(txs));
            },
        );

        group.bench_with_input(BenchmarkId::new("parallel", tx_count), &transactions, |b, txs| {
            b.iter(|| recover_senders_parallel(txs));
        });
    }

    group.finish();
}

criterion_group!(benches, sender_recovery_benches);
criterion_main!(benches);
