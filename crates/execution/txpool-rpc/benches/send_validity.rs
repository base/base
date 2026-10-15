//! Loopback HTTP latency from a signed validity request to successful pool admission.
//!
//! Runs the real node, database, RPC server, and transaction validator against the funded
//! devnet genesis. Signing, node startup, and removal between submissions are not timed.
//! Removal keeps each submission fresh rather than benchmarking the already-known shortcut.
//! No blocks are produced: this measures submission, not inclusion or remote network latency.
//! Set `VALIDITY_BENCH_EVENTS` to a JSONL path to benchmark with the event journal enabled.
//!
//! ```sh
//! RUST_LOG=error cargo bench -p base-txpool-rpc --bench send_validity --profile release
//! ```
//!
//! Use the same CPU affinity for before/after runs, and do not run builds or other benchmarks
//! concurrently. The signature policy and predicate count are part of each benchmark's name.

use std::time::{Duration, Instant};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, TxHash, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use base_execution_txpool::{ValidityAuthorization, ValidityOperator, ValidityPredicate};
use base_node_runner::{BaseNodeExtension, BaseRpcContext, NodeHooks, test_utils::TestHarness};
use base_observability_events::{
    GlobalTransactionEventWriter, TransactionEventProducer, TransactionEventWriterConfig,
};
use base_test_utils::Account;
use base_txpool_rpc::{
    AdminTxPoolApiImpl, AdminTxPoolApiServer, SendRawTransactionValidityConfig,
    SendRawTransactionValidityExtension, SendRawTransactionValidityOptions, ValiditySignatureMode,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};

/// Exposes pool removal on the isolated benchmark node's HTTP endpoint.
#[derive(Debug)]
pub struct PoolCleanup;

impl BaseNodeExtension for PoolCleanup {
    fn apply(self: Box<Self>, builder: NodeHooks) -> NodeHooks {
        builder.add_rpc_module(|ctx: &mut BaseRpcContext<'_>| {
            ctx.modules.merge_configured(AdminTxPoolApiImpl::new(ctx.pool().clone()).into_rpc())?;
            Ok(())
        })
    }
}

/// Measures small, intermediate, and maximum-sized sidecars with both signature policies.
pub fn send_validity(c: &mut Criterion) {
    if let Some(path) = std::env::var_os("VALIDITY_BENCH_EVENTS") {
        GlobalTransactionEventWriter::init(Some(TransactionEventWriterConfig {
            enabled: true,
            required: true,
            ..TransactionEventWriterConfig::disabled(
                TransactionEventProducer::BaseRethNode,
                "bench",
                path,
            )
        }))
        .unwrap();
    }
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut group = c.benchmark_group("send_validity_http");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));

    for (policy, mode) in
        [("unsigned", ValiditySignatureMode::Off), ("signed", ValiditySignatureMode::Required)]
    {
        let harness = runtime.block_on(async {
            TestHarness::builder()
                .with_ext::<SendRawTransactionValidityExtension>(SendRawTransactionValidityConfig {
                    validity_signature_mode: mode,
                    ..Default::default()
                })
                .with_extension(PoolCleanup)
                .build()
                .await
                .unwrap()
        });
        let client = HttpClientBuilder::default().build(harness.rpc_url()).unwrap();
        let tx = TxEip1559 {
            chain_id: harness.chain_id(),
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            to: TxKind::Call(Account::Bob.address()),
            ..Default::default()
        };
        let signature = Account::Alice.signer().sign_hash_sync(&tx.signature_hash()).unwrap();
        let raw: Bytes = tx.into_signed(signature).encoded_2718().into();
        let expected_hash = keccak256(&raw);

        for count in [2, 16, 64] {
            let mut validity: Vec<_> = (1..count)
                .map(|slot| ValidityPredicate::Storage {
                    address: Account::Bob.address(),
                    slot: U256::from(slot),
                    mask: U256::MAX,
                    op: ValidityOperator::Equal,
                    value: U256::ZERO,
                })
                .collect();
            validity.push(ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(31),
            });
            let validity_signature = (mode == ValiditySignatureMode::Required).then(|| {
                Account::Alice
                    .signer()
                    .sign_hash_sync(&ValidityAuthorization::signing_hash(
                        harness.chain_id(),
                        expected_hash,
                        &validity,
                    ))
                    .unwrap()
            });
            let options = SendRawTransactionValidityOptions { validity, validity_signature };

            group.bench_with_input(BenchmarkId::new(policy, count), &options, |b, options| {
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            let start = Instant::now();
                            let hash: TxHash = client
                                .request(
                                    "base_sendRawTransactionValidity",
                                    rpc_params![&raw, options],
                                )
                                .await
                                .unwrap();
                            elapsed += start.elapsed();
                            assert_eq!(hash, expected_hash);
                            let removed: bool = client
                                .request("admin_dropTransaction", rpc_params![hash])
                                .await
                                .unwrap();
                            assert!(removed, "submission must actually insert into the pool");
                        }
                        elapsed
                    })
                });
            });
        }
        // The node owns a runtime; drop it outside an async context.
        drop(harness);
    }
    group.finish();
}

criterion_group!(benches, send_validity);
criterion_main!(benches);
