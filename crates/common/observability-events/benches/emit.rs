//! Cost of `transaction_event!` at a call site when no writer is configured.
//!
//! Mempool nodes never configure a writer, so every `eth_sendRawTransaction` and
//! `base_sendRawTransactionValidity` emits into nothing. The arms mirror those call sites and the
//! builder's validated-insert event.
//!
//! ```sh
//! cargo bench -p base-observability-events --bench emit
//! ```

use std::hint::black_box;

use alloy_primitives::TxHash;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, TransactionEventWriter, transaction_event,
};
use criterion::{Criterion, criterion_group, criterion_main};

fn bench(c: &mut Criterion) {
    let tx_hash = TxHash::repeat_byte(0x42);
    let mut group = c.benchmark_group("transaction_event_unconfigured");
    group.bench_function("send_raw_transaction", |b| {
        b.iter(|| {
            transaction_event!(
                writer: Option::<&TransactionEventWriter>::None,
                producer: TransactionEventProducer::BaseRethNode,
                event_type: TransactionEventType::TxpoolSendRawTransaction,
                tx_hash: black_box(tx_hash),
                data: { "rpc_method" => "eth_sendRawTransaction" },
            )
        })
    });
    group.bench_function("validated_insert", |b| {
        b.iter(|| {
            let tx_hash = black_box(tx_hash);
            transaction_event!(
                writer: Option::<&TransactionEventWriter>::None,
                producer: TransactionEventProducer::BaseBuilder,
                event_type: TransactionEventType::TxpoolValidatedInsertAccepted,
                tx_hash: tx_hash,
                id: { "builder_host" => "builder-0", "tx_hash" => format!("{tx_hash:#x}") },
                data: { "rpc_method" => "base_insertValidatedTransaction", "encoded_len" => 110 },
            )
        })
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
