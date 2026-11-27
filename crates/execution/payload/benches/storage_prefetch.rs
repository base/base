//! Per-transaction cost the storage-trie prefetch adds to the native builder's commit path.
//!
//! The prefetch hook runs once per committed transaction, not per candidate. The `observe`
//! group measures the hook alone: a plain transfer, a token transfer below the gate (the common
//! case, including the first-sight map insert), and a 725-slot batch that triggers (the
//! enqueue only; workers discard chunks without reading). The `commit` group measures revm's
//! `State::commit` for a token transfer with no hook, which is the disabled path, and with the
//! prefetch hook installed, which also pays revm's commit-with-hook path.

use std::{hint::black_box, time::Duration};

use alloy_primitives::{Address, U256};
use base_execution_payload_builder::{
    StoragePrefetchOpener, StoragePrefetchPool, StoragePrefetchSession,
};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use reth_storage_api::errors::ProviderError;
use revm::{
    DatabaseCommit,
    database::{EmptyDB, OnStateHook, State},
    state::{Account, EvmState, EvmStorageSlot, TransactionId},
};

/// Existing slots a batched claim changes in one contract on the slowest observed blocks.
const BATCH_SLOTS: u64 = 725;

fn touched(balance: u64) -> Account {
    let mut account = Account::default();
    account.mark_touch();
    account.info.balance = U256::from(balance);
    account
}

/// Sender, recipient, and fee vaults of a plain transfer.
fn eth_transfer_state() -> EvmState {
    let mut state = EvmState::default();
    for index in 0..4u8 {
        state.insert(Address::with_last_byte(index), touched(u64::from(index) + 1));
    }
    state
}

/// A plain transfer plus a token contract changing `changed` slots and reading one.
fn token_state(changed: u64) -> EvmState {
    let mut state = eth_transfer_state();
    let mut token = touched(0);
    for slot in 0..changed {
        let mut value = EvmStorageSlot::new(U256::from(10), TransactionId::default());
        value.present_value = U256::from(9);
        token.storage.insert(U256::from(slot), value);
    }
    token.storage.insert(U256::MAX, EvmStorageSlot::new(U256::from(1), TransactionId::default()));
    state.insert(Address::repeat_byte(0x70), token);
    state
}

/// A session whose workers fail to open a reader, so queued chunks are discarded at once.
fn discarding_session(pool: &StoragePrefetchPool) -> StoragePrefetchSession {
    let opener: StoragePrefetchOpener = Box::new(|| Err(ProviderError::UnsupportedProvider));
    pool.session(opener)
}

/// A small pool with a queue deep enough that the trigger bench never drops a chunk.
fn pool() -> StoragePrefetchPool {
    StoragePrefetchPool::spawn_sized(2, 1 << 16).expect("spawn pool")
}

fn observe_benches(c: &mut Criterion) {
    let pool = pool();
    let session = discarding_session(&pool);
    session.mark_root_started();
    let mut group = c.benchmark_group("storage_prefetch_hook/observe");

    let eth = eth_transfer_state();
    let mut hook = session.hook();
    group.bench_function("eth_transfer", |b| b.iter(|| hook.observe(black_box(&eth))));

    let token = token_state(2);
    group.bench_function("token_transfer_below_gate_first_sight", |b| {
        b.iter_batched(
            || session.hook(),
            |mut hook| {
                hook.observe(black_box(&token));
                hook
            },
            BatchSize::SmallInput,
        )
    });

    // The same two slots every time, so the contract stays below the gate.
    let (_below_pool, below) = pool_session();
    let mut below_hook = below.hook();
    below_hook.observe(&token);
    group.bench_function("token_transfer_below_gate_repeat", |b| {
        b.iter(|| below_hook.observe(black_box(&token)))
    });

    let batch = token_state(BATCH_SLOTS);
    group.bench_function("trigger_725_slots_enqueue", |b| {
        b.iter_batched(
            || session.hook(),
            |mut hook| {
                hook.observe(black_box(&batch));
                hook
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

/// A pool and a session on it that discards whatever it queues.
fn pool_session() -> (StoragePrefetchPool, StoragePrefetchSession) {
    let pool = pool();
    let session = discarding_session(&pool);
    (pool, session)
}

/// A builder-style `State` with every account of `changes` loaded, as execution leaves it.
fn loaded_state(changes: &EvmState) -> State<EmptyDB> {
    let mut state = State::builder().with_database(EmptyDB::default()).with_bundle_update().build();
    for address in changes.keys() {
        state.load_cache_account(*address).expect("empty database cannot fail");
    }
    state
}

fn commit_benches(c: &mut Criterion) {
    let token = token_state(2);
    let mut group = c.benchmark_group("storage_prefetch_hook/commit");

    let mut state = loaded_state(&token);
    group.bench_function("token_transfer_no_hook", |b| {
        b.iter_batched(|| token.clone(), |changes| state.commit(changes), BatchSize::SmallInput)
    });

    let (_pool, session) = pool_session();
    let mut state = loaded_state(&token);
    state.set_state_hook(Some(Box::new(session.hook()) as Box<dyn OnStateHook>));
    group.bench_function("token_transfer_prefetch_hook", |b| {
        b.iter_batched(|| token.clone(), |changes| state.commit(changes), BatchSize::SmallInput)
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = observe_benches, commit_benches
}
criterion_main!(benches);
