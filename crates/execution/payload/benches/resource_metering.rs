//! Benchmarks for per-transaction resource-metering checks in the native payload builder.

use std::{hint::black_box, sync::Arc};

use alloy_primitives::{Address, TxHash, U256};
use base_bundles::OpcodeGas;
use base_execution_payload_builder::{
    BlockStaleTracker, NoopMeteringProvider, ResourceMeteringConfig, ResourceMeteringDimension,
    ResourceMeteringOperation, ResourceMeteringSchedule, ResourceSample, STALE_STORAGE_CACHE_MIB,
    STALE_STORAGE_CACHE_WAYS, STALE_STORAGE_CONTRACT_SLOT_GATE, STALE_STORAGE_EPOCH_SECONDS,
    StaleStorageCache, StaleStorageMeter,
};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
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

/// Block timestamp for the stale-storage benchmarks.
const BLOCK_TIMESTAMP: u64 = 1_800_000_000;

/// A default-size ([`STALE_STORAGE_CACHE_MIB`]) cache with every entry live two hours before
/// the block, under fingerprints no probed slot has, so each probe scans a full set and misses
/// and each write evicts. That is the steady state of a full cache and its slowest path.
fn filled_meter() -> StaleStorageMeter {
    let meter = StaleStorageMeter::with_cache_mib(STALE_STORAGE_CACHE_MIB);
    let hour = BLOCK_TIMESTAMP / STALE_STORAGE_EPOCH_SECONDS - 2;
    let sets = (meter.cache.len() / STALE_STORAGE_CACHE_WAYS) as u64;
    for set in 0..sets {
        for fingerprint in 1..=STALE_STORAGE_CACHE_WAYS as u64 {
            meter.cache.record((fingerprint << StaleStorageCache::FINGERPRINT_SHIFT) | set, hour);
        }
    }
    meter
}

/// One contract with `count` updated pre-existing slots, half of them cleared, as in a batched
/// XEN mint-reward claim.
fn batch_state(address: Address, count: u64) -> EvmState {
    let mut account = Account::default();
    account.mark_touch();
    for slot in 0..count {
        let present = if slot % 2 == 0 { 0 } else { 2 };
        account.storage.insert(
            U256::from(slot),
            EvmStorageSlot::new_changed(U256::from(1), U256::from(present), TransactionId::ZERO),
        );
    }
    let mut state = EvmState::default();
    state.insert(address, account);
    state
}

/// Per-transaction cost of the log-only stale-storage meter on the builder's commit path:
/// [`BlockStaleTracker::evaluate`] followed by [`BlockStaleTracker::commit`].
///
/// - `transfer_under_gate`: an ordinary token transfer in a contract under
///   [`STALE_STORAGE_CONTRACT_SLOT_GATE`], which is most transactions.
/// - `transfer_in_gated_contract`: the same transfer after the block has already written as many
///   slots of the token as the gate, so its pre-existing slots are hashed and probed.
/// - `batch_725_slots`: a 725-slot batch in one contract, the largest slow XEN shape.
fn bench_stale_storage(c: &mut Criterion) {
    let meter = filled_meter();
    let mut group = c.benchmark_group("stale_storage_evaluate_commit");
    let transfer = transfer_state();
    let token_prefill = {
        let mut account = Account::default();
        account.mark_touch();
        for slot in 1_000..1_000 + u64::from(STALE_STORAGE_CONTRACT_SLOT_GATE) {
            account.storage.insert(
                U256::from(slot),
                EvmStorageSlot::new_changed(U256::from(1), U256::from(2), TransactionId::ZERO),
            );
        }
        let mut state = EvmState::default();
        state.insert(Address::repeat_byte(0x77), account);
        state
    };
    let batch = batch_state(Address::repeat_byte(0x55), 725);

    // Earlier transactions in the block: creates in one contract and stale updates in a gated
    // one, so the per-block sets are allocated as they are after the first few transactions.
    let warmup = {
        let mut state = batch_state(Address::repeat_byte(0x98), 70);
        let mut account = Account::default();
        account.mark_touch();
        for slot in 0..32u64 {
            account.storage.insert(
                U256::from(slot),
                EvmStorageSlot::new_changed(U256::ZERO, U256::from(1), TransactionId::ZERO),
            );
        }
        state.insert(Address::repeat_byte(0x99), account);
        state
    };
    let tracker = |prefill: Option<&EvmState>| -> BlockStaleTracker {
        let mut tracker = meter.block_tracker(1, BLOCK_TIMESTAMP);
        for state in std::iter::once(&warmup).chain(prefill) {
            tracker.evaluate(state);
            tracker.commit(&TxHash::ZERO);
        }
        tracker
    };
    for (label, state, prefill) in [
        ("transfer_under_gate", &transfer, None),
        ("transfer_in_gated_contract", &transfer, Some(&token_prefill)),
        ("batch_725_slots", &batch, None),
    ] {
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter_batched(
                || tracker(prefill),
                |mut tracker| {
                    black_box(tracker.evaluate(black_box(state)));
                    black_box(tracker.commit(&TxHash::ZERO));
                    tracker
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// Cost of one recency-cache probe and one recorded write against the full default-size cache.
/// Each operation uses a fresh pseudo-random key, so it lands in a random set of the default
/// cache with a fingerprint no entry has: a full-set miss on memory that is mostly not in the
/// CPU cache.
fn bench_stale_storage_cache(c: &mut Criterion) {
    /// Odd multiplier that spreads consecutive counters over every key bit.
    const SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;
    let meter = filled_meter();
    let now_hour = BLOCK_TIMESTAMP / STALE_STORAGE_EPOCH_SECONDS;
    let mut group = c.benchmark_group("stale_storage_cache");
    group.bench_function("probe_full_set_miss", |b| {
        let mut count = 0u64;
        b.iter(|| {
            count += 1;
            black_box(meter.cache.is_stale(black_box(count.wrapping_mul(SPREAD)), now_hour))
        });
    });
    group.bench_function("record_miss_evicting", |b| {
        let mut count = 0u64;
        b.iter(|| {
            count += 1;
            black_box(meter.cache.record(black_box(count.wrapping_mul(SPREAD)), now_hour))
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_check_executed_usage,
    bench_stale_storage,
    bench_stale_storage_cache
);
criterion_main!(benches);
