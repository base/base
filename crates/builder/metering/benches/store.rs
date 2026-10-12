//! Benchmarks for [`MeteringStore`] lookups on the builder's per-candidate path.

use std::{hint::black_box, time::Duration};

use alloy_primitives::{Address, B256, TxHash, U256};
use base_builder_core::MeteringProvider;
use base_builder_metering::MeteringStore;
use base_bundles::{MeterBundleResponse, OpcodeGas, TransactionResult};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

/// Opcode rows per metered transaction: none (opcode metering off) and populated responses.
const OPCODE_ROWS: &[usize] = &[0, 16, 64];

fn response(tx_hash: TxHash, opcode_rows: usize) -> MeterBundleResponse {
    let opcode_gas = (0..opcode_rows)
        .map(|index| OpcodeGas {
            contract_address: Address::with_last_byte(index as u8),
            opcode: format!("OPCODE_{index}"),
            count: 3,
            gas_used: 300,
        })
        .collect();
    MeterBundleResponse {
        bundle_hash: B256::repeat_byte(0x11),
        results: vec![TransactionResult {
            coinbase_diff: U256::from(1),
            eth_sent_to_coinbase: U256::ZERO,
            from_address: Address::repeat_byte(0x22),
            gas_fees: U256::from(21_000),
            gas_price: U256::from(1),
            gas_used: 21_000,
            to_address: Some(Address::repeat_byte(0x33)),
            tx_hash,
            value: U256::ZERO,
            execution_time_us: 120,
            opcode_gas,
        }],
        total_gas_used: 21_000,
        total_execution_time_us: 120,
        ..Default::default()
    }
}

fn bench_get_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("metering_store_get_hit");
    for &opcode_rows in OPCODE_ROWS {
        let store = MeteringStore::new(true, 10_000, Duration::from_secs(3_600));
        let tx_hash = TxHash::repeat_byte(0x44);
        store.insert(tx_hash, response(tx_hash, opcode_rows));
        group.bench_with_input(BenchmarkId::from_parameter(opcode_rows), &tx_hash, |b, hash| {
            b.iter(|| black_box(store.get(black_box(hash))).map(|m| m.total_execution_time_us));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_get_hit);
criterion_main!(benches);
