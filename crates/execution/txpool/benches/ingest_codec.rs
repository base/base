//! Per-transaction CPU cost of the encoding work on the mempool → builder forwarding path.
//!
//! Each arm is one step a forwarded transaction pays, in path order:
//!
//! - `mempool_wire_bytes`: the forwarding reader turning a pooled transaction into wire bytes,
//!   by cloning and re-encoding it (`clone_encode`) or by reusing the pooled transaction's cached
//!   EIP-2718 bytes (`cached`), which mempool validation has already filled.
//! - `wire_serialize` / `wire_deserialize`: JSON (de)serialization of the
//!   `base_insertValidatedTransaction` parameter, with and without validity predicates.
//! - `builder_decode`: the builder decoding the EIP-2718 bytes into a pooled transaction.
//! - `builder_reencode`: the EIP-2718 encoding builder validation computes for the L1 data fee.
//! - `sender_recovery`: the signature recovery the builder skips by trusting the forwarded
//!   sender, for scale.
//!
//! ```sh
//! cargo bench -p base-execution-txpool --bench ingest_codec
//! ```

use std::hint::black_box;

use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::BaseTransactionSigned;
use base_execution_txpool::{
    BasePooledTransaction, BasePooledTx, NoExtensions, TransactionValidity, ValidatedTransaction,
    ValidityOperator, ValidityPredicate,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_primitives_traits::Recovered;
use reth_transaction_pool::PoolTransaction;

/// A transfer (no calldata) and a contract call with swap-sized calldata.
const CALLDATA_SIZES: [usize; 2] = [0, 1_024];

fn signed(calldata: usize) -> (BaseTransactionSigned, Address) {
    let signer = PrivateKeySigner::random();
    let tx = TxEip1559 {
        chain_id: 8453,
        nonce: 7,
        gas_limit: 200_000,
        max_fee_per_gas: 10_000_000_000,
        max_priority_fee_per_gas: 1_000_000,
        to: TxKind::Call(Address::repeat_byte(0x42)),
        value: U256::from(1_000),
        access_list: Default::default(),
        input: Bytes::from(vec![0xab; calldata]),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("signs");
    (tx.into_signed(signature).into(), signer.address())
}

/// A pooled transaction as the mempool holds it after validation: cached bytes filled.
fn pooled(calldata: usize) -> BasePooledTransaction {
    let (tx, sender) = signed(calldata);
    let len = tx.encode_2718_len();
    let pooled = BasePooledTransaction::new(Recovered::new_unchecked(tx, sender), len);
    let _ = BasePooledTx::encoded_2718(&pooled);
    pooled
}

fn wire<E>(calldata: usize, extensions: E) -> ValidatedTransaction<E> {
    let (tx, sender) = signed(calldata);
    ValidatedTransaction { sender, raw: tx.encoded_2718().into(), metering: None, extensions }
}

fn validity() -> TransactionValidity {
    TransactionValidity {
        validity: vec![
            ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(1_000_000),
            },
            serde_json::from_value(serde_json::json!({
                "type": "storage",
                "params": { "address": Address::repeat_byte(0x11), "slot": "0x1", "op": "=", "value": "0x2" }
            }))
            .expect("valid predicate"),
        ],
        validity_signature: None,
    }
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_wire_bytes");
    for size in CALLDATA_SIZES {
        let tx = pooled(size);
        group.bench_with_input(BenchmarkId::new("clone_encode", size), &tx, |b, tx| {
            b.iter(|| Bytes::from(black_box(tx).clone_into_consensus().inner().encoded_2718()))
        });
        group.bench_with_input(BenchmarkId::new("cached", size), &tx, |b, tx| {
            b.iter(|| BasePooledTx::encoded_2718(black_box(tx)).into_owned())
        });
    }
    group.finish();

    let mut group = c.benchmark_group("wire_serialize");
    for size in CALLDATA_SIZES {
        let plain = wire(size, NoExtensions {});
        let predicated = wire(size, validity());
        group.bench_with_input(BenchmarkId::new("no_validity", size), &plain, |b, tx| {
            b.iter(|| serde_json::to_string(black_box(tx)).expect("serializes"))
        });
        group.bench_with_input(BenchmarkId::new("validity", size), &predicated, |b, tx| {
            b.iter(|| serde_json::to_string(black_box(tx)).expect("serializes"))
        });
    }
    group.finish();

    let mut group = c.benchmark_group("wire_deserialize");
    for size in CALLDATA_SIZES {
        let plain = serde_json::to_string(&wire(size, NoExtensions {})).expect("serializes");
        let predicated = serde_json::to_string(&wire(size, validity())).expect("serializes");
        group.bench_with_input(BenchmarkId::new("no_validity", size), &plain, |b, json| {
            b.iter(|| {
                serde_json::from_str::<ValidatedTransaction<TransactionValidity>>(black_box(json))
                    .expect("deserializes")
            })
        });
        group.bench_with_input(BenchmarkId::new("validity", size), &predicated, |b, json| {
            b.iter(|| {
                serde_json::from_str::<ValidatedTransaction<TransactionValidity>>(black_box(json))
                    .expect("deserializes")
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("builder");
    for size in CALLDATA_SIZES {
        let tx = wire(size, NoExtensions {});
        group.bench_with_input(BenchmarkId::new("decode", size), &tx, |b, tx| {
            b.iter(|| {
                let decoded = BaseTransactionSigned::decode_2718(&mut black_box(&tx.raw).as_ref())
                    .expect("decodes");
                BasePooledTransaction::<BaseTransactionSigned>::new(
                    Recovered::new_unchecked(decoded, tx.sender),
                    tx.raw.len(),
                )
            })
        });
        group.bench_with_input(BenchmarkId::new("reencode", size), &tx, |b, tx| {
            let decoded =
                BaseTransactionSigned::decode_2718(&mut tx.raw.as_ref()).expect("decodes");
            b.iter(|| black_box(&decoded).encoded_2718())
        });
        group.bench_with_input(BenchmarkId::new("sender_recovery", size), &tx, |b, tx| {
            let decoded =
                BaseTransactionSigned::decode_2718(&mut tx.raw.as_ref()).expect("decodes");
            b.iter(|| black_box(&decoded).recover_signer().expect("recovers"))
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
