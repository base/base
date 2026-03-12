//! Benchmarks for [`WebSocketPublisher`] publish throughput.

use std::{hint::black_box, net::SocketAddr, time::Duration};

use base_builder_publish::WebSocketPublisher;
use criterion::{Criterion, criterion_group, criterion_main};
use futures::StreamExt;
use tokio::runtime::Runtime;

/// Binds an OS-assigned port, returns the address, then drops the listener so
/// the publisher can rebind.
fn ephemeral_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

/// Builds a flashblock-like JSON payload of approximately `tx_count` transactions.
/// Each transaction is ~512 hex chars plus a receipt with one log entry.
fn payload_with_txs(tx_count: usize) -> serde_json::Value {
    let txs: Vec<_> = (0..tx_count).map(|i| format!("0x{i:0>512}")).collect();
    let receipts: Vec<_> = (0..tx_count)
        .map(|i| {
            serde_json::json!({
                "status": "0x1",
                "cumulative_gas_used": format!("0x{:x}", i * 21000),
                "logs": [{
                    "address": format!("0x{}", "ab".repeat(20)),
                    "data": format!("0x{}", "cd".repeat(128)),
                    "topics": [format!("0x{}", "ef".repeat(32))]
                }]
            })
        })
        .collect();
    serde_json::json!({
        "index": 0,
        "metadata": {
            "block_number": 12345678,
            "flashblock_index": 0,
            "timestamp": 1700000000,
            "parent_hash": format!("0x{}", "a".repeat(64)),
            "base_fee_per_gas": "0x3b9aca00"
        },
        "diff": {
            "state_root": format!("0x{}", "b".repeat(64)),
            "receipts_root": format!("0x{}", "c".repeat(64)),
            "logs_bloom": format!("0x{}", "d".repeat(512)),
            "gas_used": "0x5208",
            "transactions": txs,
            "receipts": receipts
        }
    })
}

/// Returns `(label, payload)` tuples for the three size tiers.
fn sized_payloads() -> Vec<(&'static str, serde_json::Value)> {
    // Approximate sizes: 5 txs ≈ 5 KB, 50 txs ≈ 50 KB, 500 txs ≈ 500 KB
    vec![
        ("5kb", payload_with_txs(5)),
        ("50kb", payload_with_txs(50)),
        ("500kb", payload_with_txs(500)),
    ]
}

/// Creates a publisher with `n` connected clients, each draining messages.
fn publisher_with_subscribers(rt: &Runtime, n: usize) -> WebSocketPublisher {
    rt.block_on(async {
        let addr = ephemeral_addr();
        let publisher = WebSocketPublisher::with_capacity(addr, 100, 16).unwrap();

        for _ in 0..n {
            let (client, _) =
                tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
            let (_, mut read) = client.split();
            tokio::spawn(async move { while read.next().await.is_some() {} });
        }

        if n > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        publisher
    })
}

/// Benchmarks `publish()` (serialize + broadcast) across payload sizes and subscriber counts.
fn bench_publish(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let payloads = sized_payloads();

    for &(label, ref payload) in &payloads {
        let size = serde_json::to_string(payload).unwrap().len();
        eprintln!("{label} payload: {size} bytes ({:.1} KB)", size as f64 / 1024.0);
    }

    for sub_count in [0, 1, 10] {
        let publisher = publisher_with_subscribers(&rt, sub_count);
        for &(label, ref payload) in &payloads {
            c.bench_function(&format!("publish/{label}/{sub_count}sub"), |b| {
                b.iter(|| {
                    black_box(publisher.publish(payload, 1, 0).unwrap());
                });
            });
        }
    }
}

criterion_group!(benches, bench_publish);
criterion_main!(benches);
