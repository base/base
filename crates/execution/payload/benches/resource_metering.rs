//! Benchmarks for per-transaction resource-metering checks in the native payload builder.

use std::{hint::black_box, sync::Arc};

use alloy_primitives::{Address, TxHash, U256};
use base_bundles::OpcodeGas;
use base_execution_payload_builder::{
    ContractSlotLimits, ContractSlotUsage, NoopMeteringProvider, ResourceMeteringConfig,
    ResourceMeteringDimension, ResourceMeteringOperation, ResourceMeteringSchedule, ResourceSample,
    TrieDepthClass, TrieDepthTable,
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
                    &mut ContractSlotUsage::default(),
                ))
            });
        });
    }
    group.finish();
}

/// Per-contract limits to compare against a schedule without them.
#[derive(Clone, Copy)]
enum ContractSlotsMode {
    Disabled,
    FlatCap,
    DepthTable,
}

fn owner(index: usize) -> Address {
    let mut bytes = [0u8; 20];
    bytes[..8].copy_from_slice(&(index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    bytes[12..].copy_from_slice(&(index as u64).to_be_bytes());
    Address::from(bytes)
}

/// The CPU dimension plus optional per-contract limits. The depth table lists owners
/// `0..2_331` across classes 4 to 8 in the probed Mainnet counts.
fn contract_slots_config(mode: ContractSlotsMode) -> ResourceMeteringConfig {
    let base = config(true);
    let mut schedule = ResourceMeteringSchedule::new(base.schedule.dimensions.clone());
    schedule.contract_slots = match mode {
        ContractSlotsMode::Disabled => None,
        ContractSlotsMode::FlatCap => Some(ContractSlotLimits::new(Some(600), None)),
        ContractSlotsMode::DepthTable => {
            let mut next = 0;
            let classes = [(8u8, 2usize), (7, 14), (6, 111), (5, 449), (4, 1_755)]
                .into_iter()
                .map(|(depth, count)| {
                    let contracts = (next..next + count).map(owner).collect();
                    next += count;
                    TrieDepthClass { depth, contracts }
                })
                .collect();
            Some(ContractSlotLimits::new(
                Some(2_000),
                Some(TrieDepthTable { node_budget: 2_100, classes }),
            ))
        }
    };
    ResourceMeteringConfig { schedule: Arc::new(schedule.compile().unwrap()), ..base }
}

/// Post-state with `slots_per_owner` changed slots in each of `owners` storage tries.
/// Owner 0 is a listed depth-8 contract; the rest alternate listed and unlisted.
fn storage_state(owners: usize, slots_per_owner: u64) -> EvmState {
    let mut state = transfer_state();
    for index in 0..owners {
        let mut account = Account::default();
        account.mark_touch();
        for slot in 0..slots_per_owner {
            account.storage.insert(
                U256::from(slot),
                EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), TransactionId::ZERO),
            );
        }
        let address = if index % 2 == 0 { owner(index) } else { owner(1_000_000 + index) };
        state.insert(address, account);
    }
    state
}

/// Block totals after `owners` storage tries were committed earlier in the block.
fn committed_usage(config: &ResourceMeteringConfig, owners: usize) -> ContractSlotUsage {
    let mut usage = ContractSlotUsage::default();
    for index in 0..owners {
        usage.staged.push((owner(index * 3), 5));
    }
    config.commit_contract_slots(&TxHash::ZERO, &mut usage);
    usage
}

/// One executed attempt: the evaluator check plus per-contract staging and check.
fn bench_contract_slots_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("resource_metering_contract_slots_check");
    let simulated = simulated();
    let states = [
        ("transfer", transfer_state()),
        ("one_owner_700_slots", storage_state(1, 700)),
        ("30_owners_10_slots", storage_state(30, 10)),
    ];
    for (mode_label, mode) in [
        ("disabled", ContractSlotsMode::Disabled),
        ("flat_cap", ContractSlotsMode::FlatCap),
        ("depth_table", ContractSlotsMode::DepthTable),
    ] {
        let config = contract_slots_config(mode);
        for (block_label, committed_owners) in
            [("block_28_owners", 28), ("block_1032_owners", 1_032)]
        {
            let mut usage = committed_usage(&config, committed_owners);
            for (state_label, state) in &states {
                group.bench_function(
                    BenchmarkId::new(mode_label, format!("{state_label}/{block_label}")),
                    |b| {
                        b.iter(|| {
                            black_box(config.check_executed_usage(
                                &TxHash::ZERO,
                                52_000,
                                black_box(state),
                                Some(black_box(&simulated)),
                                &[],
                                &mut usage,
                            ))
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

/// Committing one included transaction's staged per-contract counts.
fn bench_contract_slots_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("resource_metering_contract_slots_commit");
    let config = contract_slots_config(ContractSlotsMode::DepthTable);
    for (state_label, state) in [
        ("transfer", transfer_state()),
        ("one_owner_700_slots", storage_state(1, 700)),
        ("30_owners_10_slots", storage_state(30, 10)),
    ] {
        let mut usage = committed_usage(&config, 1_032);
        group.bench_function(BenchmarkId::from_parameter(state_label), |b| {
            b.iter(|| {
                usage.stage(black_box(&state));
                config.commit_contract_slots(&TxHash::ZERO, &mut usage);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_check_executed_usage,
    bench_contract_slots_check,
    bench_contract_slots_commit
);
criterion_main!(benches);
