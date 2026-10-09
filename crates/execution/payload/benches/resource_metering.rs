//! Benchmarks for per-transaction resource-metering checks in the native payload builder.

use std::{hint::black_box, sync::Arc};

use alloy_primitives::{Address, TxHash, U256};
use base_bundles::OpcodeGas;
use base_execution_payload_builder::{
    NoopMeteringProvider, ResourceMeteringConfig, ResourceMeteringDimension,
    ResourceMeteringOperation, ResourceMeteringSchedule, ResourceSample,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use revm::state::{Account, EvmState, EvmStorageSlot, TransactionId};

fn operation(name: &str, count_cost: u64) -> ResourceMeteringOperation {
    ResourceMeteringOperation { name: name.to_string(), gas_used_weight: 1, count_cost }
}

fn dimension(name: &str, operations: Vec<ResourceMeteringOperation>) -> ResourceMeteringDimension {
    ResourceMeteringDimension {
        name: name.to_string(),
        block_limit: u64::MAX,
        transaction_limit: u64::MAX,
        base_gas_weight: 1,
        operations,
        dry_run: false,
    }
}

/// A CPU dimension over common opcodes, optionally with a state-growth dimension.
fn config(price_state_growth: bool) -> ResourceMeteringConfig {
    let mut dimensions = vec![dimension(
        "cpu",
        ["SLOAD", "SSTORE", "KECCAK256", "CALL", "LOG3"]
            .into_iter()
            .map(|name| operation(name, 10))
            .collect(),
    )];
    if price_state_growth {
        dimensions.push(dimension(
            "stateGrowth",
            vec![operation(ResourceSample::STATE_NEW_STORAGE_SLOT, 1_000)],
        ));
    }
    ResourceMeteringConfig {
        enabled: true,
        schedule: Arc::new(ResourceMeteringSchedule::new(dimensions).compile().unwrap()),
        provider: Arc::new(NoopMeteringProvider),
    }
}

/// Post-state of a token transfer: sender, token contract with written slots, and fee recipients.
fn transfer_state() -> EvmState {
    let mut state = EvmState::default();
    for index in 0..4u8 {
        let mut account = Account::default();
        account.mark_touch();
        account.info.balance = U256::from(index + 1);
        state.insert(Address::with_last_byte(index), account);
    }
    let mut token = Account::default();
    token.mark_touch();
    for (slot, original, present) in [(1u64, 0u64, 7u64), (2, 5, 3), (3, 4, 0), (4, 9, 9)] {
        token.storage.insert(
            U256::from(slot),
            EvmStorageSlot::new_changed(
                U256::from(original),
                U256::from(present),
                TransactionId::ZERO,
            ),
        );
    }
    state.insert(Address::repeat_byte(0x77), token);
    state
}

fn simulated() -> ResourceSample {
    ResourceSample {
        gas_used: 52_000,
        operations: ["SLOAD", "SSTORE", "KECCAK256", "CALL", "LOG3", "MSTORE", "ADD", "JUMPI"]
            .into_iter()
            .map(|opcode| OpcodeGas {
                contract_address: Address::repeat_byte(0x77),
                opcode: opcode.to_string(),
                count: 4,
                gas_used: 400,
            })
            .collect(),
    }
}

fn bench_check_executed_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("resource_metering_check_executed_usage");
    let state = transfer_state();
    let simulated = simulated();
    for (label, price_state_growth) in [("state_priced", true), ("state_unpriced", false)] {
        let config = config(price_state_growth);
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                black_box(config.check_executed_usage(
                    &TxHash::ZERO,
                    52_000,
                    black_box(&state),
                    Some(black_box(&simulated)),
                    &[],
                ))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_check_executed_usage);
criterion_main!(benches);
