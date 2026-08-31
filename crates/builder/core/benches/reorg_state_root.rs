//! Benchmark: state root cost after a reorg gap, cold recompute vs warm cached-node sync.
//!
//! Models the shadow-builder reconciliation problem. After a reorg the builder must
//! produce the state root for the block that lands on top of an `N`-block gap of
//! canonical blocks it did not build. Two strategies are compared:
//!
//! - `cold` — [`StateRootProvider::state_root_with_updates`] over the full accumulated
//!   [`HashedPostState`] for the gap. This is the current builder behavior
//!   ([`crates/builder/core/src/flashblocks/payload.rs`] `build_block`): every gap
//!   change lands in the prefix set, so the trie is re-walked from the root for the
//!   whole gap.
//!
//! - `warm` — [`StateRootProvider::state_root_from_nodes_with_updates`] fed a
//!   [`TrieInput`] whose `nodes` carry each gap block's already-computed
//!   [`TrieUpdates`] via [`TrieInput::append_cached`], with only the final (tip) block's
//!   state driving the prefix set via [`TrieInput::append`]. Gap subtrees are served
//!   from cached nodes instead of re-walked.
//!
//! Both strategies MUST produce the identical root — the bench asserts this before
//! timing, so a passing run is also a correctness proof that the warm path is a valid
//! substitute for the cold path.
//!
//! The gap blocks are committed to a RocksDB-backed store exactly as production does,
//! so the cold path exercises real disk I/O through the trie overlay and the warm
//! path's cached nodes are the real per-block `TrieUpdates` returned when each block
//! was sealed.
use std::{hint::black_box, sync::Arc};

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent, NumHash};
use alloy_primitives::{B256, U256, keccak256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStore, BaseProofsStorage, RocksdbProofsStorage,
    provider::BaseProofsStateProviderRef,
    api::BlockStateDiff,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_provider::{StateRootProvider, noop::NoopProvider};
use reth_trie_common::{HashedPostState, HashedStorage, TrieInput};
use tempfile::TempDir;

/// Storage slots per touched account.
const SLOTS_PER_ACCOUNT: usize = 5;

/// Accounts changed per gap block.
const ACCOUNTS_PER_BLOCK: usize = 200;

/// Pre-existing accounts in the base state (realistic non-empty trie).
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

/// Creates a RocksDB-backed proofs storage pre-populated with [`BASE_STATE_ACCOUNTS`]
/// accounts at block 0. Returns the temp dir (keep alive) and the wrapped storage.
fn create_populated_storage() -> (TempDir, BaseProofsStorage<Arc<RocksdbProofsStorage>>) {
    let dir = TempDir::new().expect("failed to create temp dir");
    let rocksdb =
        Arc::new(RocksdbProofsStorage::new(dir.path()).expect("failed to create RocksDB storage"));

    let mut rng = StdRng::seed_from_u64(42);
    let accounts = generate_accounts(&mut rng, BASE_STATE_ACCOUNTS, SLOTS_PER_ACCOUNT);

    let hashed_accounts: Vec<(B256, Option<Account>)> =
        accounts.iter().map(|(addr, acct, _)| (*addr, Some(*acct))).collect();
    rocksdb.store_hashed_accounts(hashed_accounts).expect("failed to store hashed accounts");

    for (addr, _, slots) in &accounts {
        let slot_data: Vec<(B256, U256)> = slots.iter().map(|(k, v)| (*k, *v)).collect();
        rocksdb.store_hashed_storages(*addr, slot_data).expect("failed to store hashed storages");
    }

    rocksdb
        .set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))
        .expect("failed to set anchor");
    rocksdb.commit_initial_state().expect("failed to commit initial state");

    (dir, BaseProofsStorage::from(rocksdb))
}

/// Result of building and committing a gap of `n` blocks on top of the base state.
struct Gap {
    /// Per-block cached `(TrieUpdates, HashedPostState)` captured as each block sealed,
    /// oldest first. The last entry is the tip block.
    per_block: Vec<(reth_trie_common::updates::TrieUpdates, HashedPostState)>,
    /// The full accumulated hashed state across all gap blocks (cold-path input).
    accumulated: HashedPostState,
    /// The state root after the final gap block.
    final_root: B256,
    /// Block number of the tip block.
    tip_block: u64,
}

/// Builds `n` gap blocks, committing each to `storage` with its real computed trie
/// updates, and returns the material needed to drive both cold and warm strategies.
///
/// Block `i` (1-based) changes a fresh set of accounts, its root computed against the
/// state committed for blocks `0..i`, mirroring how the live writer seals blocks.
fn build_and_commit_gap<S: BaseProofsStore + Clone>(
    storage: &BaseProofsStorage<S>,
    n: u64,
    rng: &mut StdRng,
) -> Gap {
    let mut per_block = Vec::with_capacity(n as usize);
    let mut accumulated = HashedPostState::default();
    let mut final_root = B256::ZERO;
    // Blocks are hash-chained: block 1's parent is the anchor hash (ZERO), then each
    // block's parent is the previous block's hash. The store enforces this ordering.
    let mut parent_hash = B256::ZERO;

    for block in 1..=n {
        let accounts = generate_accounts(rng, ACCOUNTS_PER_BLOCK, SLOTS_PER_ACCOUNT);
        let delta = build_hashed_post_state(&accounts);

        // Compute this block's root+updates against state as of the previous block.
        let provider =
            BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), storage, block - 1);
        let (root, updates) = provider
            .state_root_with_updates(delta.clone())
            .expect("gap block state root should succeed");
        final_root = root;

        let block_hash = keccak256(block.to_be_bytes());
        storage
            .store_trie_updates(
                BlockWithParent::new(parent_hash, NumHash::new(block, block_hash)),
                BlockStateDiff {
                    sorted_trie_updates: updates.clone().into_sorted(),
                    sorted_post_state: delta.clone().into_sorted(),
                },
            )
            .expect("commit gap block");
        parent_hash = block_hash;

        accumulated.extend(delta.clone());
        per_block.push((updates, delta));
    }

    Gap { per_block, accumulated, final_root, tip_block: n }
}

/// Cold strategy: full accumulated state, prefix set covers the whole gap.
fn cold_root<S: BaseProofsStore + Clone>(
    storage: &BaseProofsStorage<S>,
    gap: &Gap,
) -> B256 {
    let provider =
        BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), storage, gap.tip_block);
    let (root, _updates) = provider
        .state_root_with_updates(gap.accumulated.clone())
        .expect("cold state root should succeed");
    root
}

/// Warm strategy: gap blocks supplied as cached nodes, only the tip block's state
/// drives the prefix set.
fn warm_root<S: BaseProofsStore + Clone>(
    storage: &BaseProofsStorage<S>,
    gap: &Gap,
) -> B256 {
    let mut input = TrieInput::default();
    // All but the tip contribute cached nodes (no prefix-set entries).
    let (tip, older) = gap.per_block.split_last().expect("gap has >= 1 block");
    for (nodes, state) in older {
        input.append_cached_ref(nodes, state);
    }
    // Tip block's own nodes are cached too, but its state drives the walk.
    input.append_cached_ref(&tip.0, &tip.1);
    input.append_ref(&tip.1);

    let provider =
        BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), storage, gap.tip_block);
    let (root, _updates) = provider
        .state_root_from_nodes_with_updates(input)
        .expect("warm state root should succeed");
    root
}

fn reorg_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("reorg_state_root");
    group.sample_size(10);

    for &gap_len in &[10u64, 15, 20] {
        let (_dir, storage) = create_populated_storage();
        let mut rng = StdRng::seed_from_u64(1337 + gap_len);
        let gap = build_and_commit_gap(&storage, gap_len, &mut rng);

        // Correctness gate: warm must equal cold must equal the sealed final root.
        let cold = cold_root(&storage, &gap);
        let warm = warm_root(&storage, &gap);
        assert_eq!(cold, gap.final_root, "cold root diverged from sealed root (gap={gap_len})");
        assert_eq!(warm, cold, "warm root diverged from cold root (gap={gap_len})");

        group.bench_function(BenchmarkId::new("cold", gap_len), |b| {
            b.iter(|| black_box(cold_root(&storage, &gap)));
        });
        group.bench_function(BenchmarkId::new("warm", gap_len), |b| {
            b.iter(|| black_box(warm_root(&storage, &gap)));
        });
    }

    group.finish();
}

criterion_group!(benches, reorg_benches);
criterion_main!(benches);
