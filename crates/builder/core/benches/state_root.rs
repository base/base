//! Benchmarks for state root computation during block building.
//!
//! Measures the cost of the two `calculate_state_root` modes with 10
//! flashblocks at varying accounts-per-flashblock sizes:
//!
//! - `finalize_only` (`calculate_state_root = false`) — state root computed
//!   once over the full accumulated state at finalization. This is the
//!   **current production behavior**.
//!
//! - `per_flashblock` (`calculate_state_root = true`) — state root
//!   recomputed from scratch after every flashblock.
//!
//! The benchmarks use an MDBX-backed [`MdbxProofsStorage`] pre-populated with
//! 50k base-state accounts so that state root computation exercises real disk
//! I/O through the production trie overlay path.
//!
//! [`StateRootProvider::state_root_with_updates`] takes [`HashedPostState`] by
//! value, so each iteration clones the post state. `finalize_only` clones the
//! full accumulated state once; `per_flashblock` clones one snapshot per
//! flashblock (sizes 1×…10× a single delta), so copy work in the per-flashblock
//! loop is higher than in `finalize_only` and contributes to the reported gap
//! alongside repeated trie work.
use std::{hint::black_box, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256, keccak256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, MdbxProofsStorage,
    provider::BaseProofsStateProviderRef,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_provider::{StateRootProvider, noop::NoopProvider};
use reth_trie_common::{HashedPostState, HashedStorage};
use tempfile::TempDir;

/// Number of flashblocks per block.
const FLASHBLOCKS: usize = 10;

/// Storage slots per account.
const SLOTS_PER_ACCOUNT: usize = 5;

/// Number of pre-existing accounts in the MDBX base state.
///
/// This simulates a realistic chain state where the trie already contains
/// existing accounts before any flashblock deltas are applied.
const BASE_STATE_ACCOUNTS: usize = 50_000;

/// A hashed address, its account data, and its storage slots.
type HashedAccountData = (B256, Account, Vec<(B256, U256)>);

/// Generates `n` deterministic (`hashed_address`, account, `storage_slots`) tuples.
fn generate_accounts(
    rng: &mut StdRng,
    n: usize,
    slots_per_account: usize,
) -> Vec<HashedAccountData> {
    (0..n)
        .map(|_| {
            let addr = keccak256(rng.random::<[u8; 20]>());
            let account = Account {
                nonce: rng.random::<u64>(),
                balance: U256::from(rng.random::<u64>()),
                bytecode_hash: None,
            };
            let slots: Vec<(B256, U256)> = (0..slots_per_account)
                .map(|_| (keccak256(rng.random::<[u8; 32]>()), U256::from(rng.random::<u64>())))
                .collect();
            (addr, account, slots)
        })
        .collect()
}

/// Builds a [`HashedPostState`] from generated accounts.
fn build_hashed_post_state(accounts: &[HashedAccountData]) -> HashedPostState {
    let mut state = HashedPostState::default();
    for (addr, account, slots) in accounts {
        state.accounts.insert(*addr, Some(*account));
        let mut storage = HashedStorage::new(false);
        for (slot, value) in slots {
            storage.storage.insert(*slot, *value);
        }
        state.storages.insert(*addr, storage);
    }
    state
}

/// Pre-generates per-flashblock account changesets and the full accumulated state.
fn setup_flashblock_data(
    accounts_per_flashblock: usize,
) -> (Vec<HashedPostState>, HashedPostState) {
    // Use a different seed range to avoid overlap with base state accounts.
    let mut rng = StdRng::seed_from_u64(1337);
    let mut deltas = Vec::with_capacity(FLASHBLOCKS);
    let mut full_state = HashedPostState::default();

    for _ in 0..FLASHBLOCKS {
        let accounts = generate_accounts(&mut rng, accounts_per_flashblock, SLOTS_PER_ACCOUNT);
        let delta = build_hashed_post_state(&accounts);
        full_state.extend(delta.clone());
        deltas.push(delta);
    }

    (deltas, full_state)
}

/// Creates an MDBX-backed proofs storage pre-populated with [`BASE_STATE_ACCOUNTS`]
/// accounts and their storage slots. Returns the temp directory handle (must be
/// kept alive) and the wrapped storage.
fn create_populated_storage() -> (TempDir, BaseProofsStorage<Arc<MdbxProofsStorage>>) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let mdbx = Arc::new(MdbxProofsStorage::new(dir.path()).expect("failed to create MDBX storage"));

    // Generate base state accounts.
    let mut rng = StdRng::seed_from_u64(42);
    let accounts = generate_accounts(&mut rng, BASE_STATE_ACCOUNTS, SLOTS_PER_ACCOUNT);

    // Pre-populate hashed accounts.
    let hashed_accounts: Vec<(B256, Option<Account>)> =
        accounts.iter().map(|(addr, acct, _)| (*addr, Some(*acct))).collect();
    mdbx.store_hashed_accounts(hashed_accounts).expect("failed to store hashed accounts");

    // Pre-populate hashed storages.
    for (addr, _, slots) in &accounts {
        let slot_data: Vec<(B256, U256)> = slots.iter().map(|(k, v)| (*k, *v)).collect();
        mdbx.store_hashed_storages(*addr, slot_data).expect("failed to store hashed storages");
    }

    // Set initial state anchor and commit.
    mdbx.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO)).expect("failed to set anchor");
    mdbx.commit_initial_state().expect("failed to commit initial state");

    let storage = BaseProofsStorage::from(mdbx);
    (dir, storage)
}

/// Benchmarks `calculate_state_root = false`: state root computed once at
/// finalization over the full accumulated state (current production behavior).
fn finalize_only_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_root/finalize_only");
    g.sample_size(10);

    for &accounts_per_fb in &[10, 100, 1_000] {
        let (_deltas, full_state) = setup_flashblock_data(accounts_per_fb);
        let (_dir, storage) = create_populated_storage();
        let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

        g.bench_function(BenchmarkId::new("accounts_per_fb", accounts_per_fb), |b| {
            b.iter(|| {
                provider
                    .state_root_with_updates(full_state.clone())
                    .expect("state root should succeed")
            });
        });
    }

    g.finish();
}

/// Benchmarks `calculate_state_root = true`: state root recomputed from
/// scratch after every flashblock.
fn per_flashblock_benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("state_root/per_flashblock");
    g.sample_size(10);

    for &accounts_per_fb in &[10, 100, 1_000] {
        let (deltas, _full_state) = setup_flashblock_data(accounts_per_fb);

        // Pre-build the accumulated snapshots so clone overhead doesn't
        // inflate the measured time — we only want to measure state root
        // computation, not data copying.
        let mut accumulated = HashedPostState::default();
        let snapshots: Vec<HashedPostState> = deltas
            .iter()
            .map(|delta| {
                accumulated.extend(delta.clone());
                accumulated.clone()
            })
            .collect();

        let (_dir, storage) = create_populated_storage();
        let provider = BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &storage, 0);

        g.bench_function(BenchmarkId::new("accounts_per_fb", accounts_per_fb), |b| {
            b.iter(|| {
                for snapshot in &snapshots {
                    black_box(
                        provider
                            .state_root_with_updates(snapshot.clone())
                            .expect("state root should succeed"),
                    );
                }
            });
        });
    }

    g.finish();
}

criterion_group!(benches, finalize_only_benches, per_flashblock_benches);
criterion_main!(benches);
