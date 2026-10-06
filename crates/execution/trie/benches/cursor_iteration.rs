//! Benchmarks sequential `RocksDB` versioned-cursor iteration, the access pattern reth's trie
//! walkers and proof/witness generation drive through the proofs-history store.
//!
//! The fixture seeds accounts, one account's storage, the account trie, and one storage trie,
//! then applies several blocks of random updates so keys carry multiple versions, with some
//! tombstoned accounts and trie nodes and some zero-valued storage slots. Two layouts are
//! measured: everything flushed and compacted into SST files, and a compacted base with the
//! later blocks still in the memtable (reads merge both sources).
//!
//! Each benchmark opens a fresh cursor, then either walks `WALK_LEN` consecutive live keys via
//! `next()` or performs `SEEKS` random seeks each followed by `NEXTS_PER_SEEK` `next()` calls.
//! Both a latest and a historical `max_block_number` are measured; the historical bound makes
//! the newest row of many keys invisible, exercising the bounded-version lookup.

use std::{hint::black_box, sync::Arc};

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStore, BlockStateDiff, RocksdbProofsStorage,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand_08::{Rng, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_trie::{
    BranchNodeCompact, HashedPostState, HashedStorage, Nibbles,
    hashed_cursor::HashedCursor,
    trie_cursor::TrieCursor,
    updates::{StorageTrieUpdates, TrieUpdates},
};
use tempfile::TempDir;

const ACCOUNTS: usize = 20_000;
const SLOTS: usize = 8_192;
const TRIE_PATHS: usize = 8_192;
const BLOCKS: u64 = 6;
const UPDATE_PROBABILITY: f64 = 0.5;
const TOMBSTONE_PROBABILITY: f64 = 0.03;
const ZERO_SLOT_PROBABILITY: f64 = 0.1;
const WALK_LEN: usize = 1_000;
const SEEKS: usize = 64;
const NEXTS_PER_SEEK: usize = 16;

struct Fixture {
    _dir: TempDir,
    storage: Arc<RocksdbProofsStorage>,
    storage_address: B256,
    accounts: Vec<B256>,
    slots: Vec<B256>,
    account_paths: Vec<Nibbles>,
    storage_paths: Vec<Nibbles>,
}

fn random_hash(rng: &mut StdRng) -> B256 {
    B256::from(rng.r#gen::<[u8; 32]>())
}

fn random_paths(rng: &mut StdRng, count: usize) -> Vec<Nibbles> {
    let mut paths: Vec<Nibbles> = (0..count)
        .map(|_| {
            let len = rng.gen_range(1..=6);
            Nibbles::from_nibbles((0..len).map(|_| rng.gen_range(0..16u8)).collect::<Vec<_>>())
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn account(rng: &mut StdRng, nonce: u64) -> Account {
    Account { nonce, balance: U256::from(rng.r#gen::<u64>()), bytecode_hash: None }
}

fn branch(rng: &mut StdRng) -> BranchNodeCompact {
    BranchNodeCompact::new(rng.r#gen::<u16>() | 1, 0, 0, vec![], Some(random_hash(rng)))
}

fn slot_value(rng: &mut StdRng) -> U256 {
    if rng.gen_bool(ZERO_SLOT_PROBABILITY) {
        U256::ZERO
    } else {
        U256::from(rng.r#gen::<u64>() | 1)
    }
}

fn block_hash(number: u64) -> B256 {
    B256::left_padding_from(&number.to_be_bytes())
}

fn block_diff(rng: &mut StdRng, fixture: &Fixture, block: u64) -> BlockStateDiff {
    let mut post_state = HashedPostState::default();
    for key in &fixture.accounts {
        if rng.gen_bool(UPDATE_PROBABILITY) {
            let value = (!rng.gen_bool(TOMBSTONE_PROBABILITY)).then(|| account(rng, block));
            post_state.accounts.insert(*key, value);
        }
    }
    let mut storage = HashedStorage::new(false);
    for slot in &fixture.slots {
        if rng.gen_bool(UPDATE_PROBABILITY) {
            storage.storage.insert(*slot, slot_value(rng));
        }
    }
    post_state.storages.insert(fixture.storage_address, storage);

    let mut trie_updates = TrieUpdates::default();
    for path in &fixture.account_paths {
        if rng.gen_bool(UPDATE_PROBABILITY) {
            if rng.gen_bool(TOMBSTONE_PROBABILITY) {
                trie_updates.removed_nodes.insert(*path);
            } else {
                trie_updates.account_nodes.insert(*path, branch(rng));
            }
        }
    }
    let mut storage_trie = StorageTrieUpdates::default();
    for path in &fixture.storage_paths {
        if rng.gen_bool(UPDATE_PROBABILITY) {
            if rng.gen_bool(TOMBSTONE_PROBABILITY) {
                storage_trie.removed_nodes.insert(*path);
            } else {
                storage_trie.storage_nodes.insert(*path, branch(rng));
            }
        }
    }
    trie_updates.storage_tries.insert(fixture.storage_address, storage_trie);

    BlockStateDiff {
        sorted_trie_updates: trie_updates.into_sorted(),
        sorted_post_state: post_state.into_sorted(),
    }
}

/// Builds the fixture. With `recent_in_memtable`, only the initial state and the first half of
/// the blocks are compacted into SST files; the remaining blocks stay in the memtable.
fn create_fixture(recent_in_memtable: bool) -> Fixture {
    let mut rng = StdRng::seed_from_u64(7);
    let dir = TempDir::new().expect("create temp dir");
    let storage = Arc::new(RocksdbProofsStorage::new(dir.path()).expect("open RocksDB"));
    let mut accounts: Vec<B256> = (0..ACCOUNTS).map(|_| random_hash(&mut rng)).collect();
    accounts.sort();
    let mut slots: Vec<B256> = (0..SLOTS).map(|_| random_hash(&mut rng)).collect();
    slots.sort();
    let fixture = Fixture {
        _dir: dir,
        storage,
        storage_address: random_hash(&mut rng),
        accounts,
        slots,
        account_paths: random_paths(&mut rng, TRIE_PATHS),
        storage_paths: random_paths(&mut rng, TRIE_PATHS),
    };

    let store = &fixture.storage;
    store.set_initial_state_anchor(BlockNumHash::new(0, block_hash(0))).expect("set anchor");
    store
        .store_hashed_accounts(
            fixture.accounts.iter().map(|key| (*key, Some(account(&mut rng, 0)))).collect(),
        )
        .expect("store accounts");
    store
        .store_hashed_storages(
            fixture.storage_address,
            fixture.slots.iter().map(|slot| (*slot, slot_value(&mut rng))).collect(),
        )
        .expect("store slots");
    store
        .store_account_branches(
            fixture.account_paths.iter().map(|path| (*path, Some(branch(&mut rng)))).collect(),
        )
        .expect("store account branches");
    store
        .store_storage_branches(
            fixture.storage_address,
            fixture.storage_paths.iter().map(|path| (*path, Some(branch(&mut rng)))).collect(),
        )
        .expect("store storage branches");
    store.commit_initial_state().expect("commit initial state");

    for number in 1..=BLOCKS {
        let block_ref =
            BlockWithParent::new(block_hash(number - 1), NumHash::new(number, block_hash(number)));
        let diff = block_diff(&mut rng, &fixture, number);
        store.store_trie_updates(block_ref, diff).expect("store block");
        if recent_in_memtable && number == BLOCKS / 2 {
            store.flush_and_compact().expect("flush and compact");
        }
    }
    if !recent_in_memtable {
        store.flush_and_compact().expect("flush and compact");
    }
    fixture
}

fn walk_hashed<C: HashedCursor>(mut cursor: C) -> usize {
    let mut count = usize::from(cursor.seek(B256::ZERO).expect("seek").is_some());
    while count < WALK_LEN {
        let Some(entry) = cursor.next().expect("next") else { break };
        black_box(entry);
        count += 1;
    }
    count
}

fn seek_next_hashed<C: HashedCursor>(mut cursor: C, targets: &[B256]) -> usize {
    let mut count = 0;
    for target in targets {
        if cursor.seek(*target).expect("seek").is_none() {
            continue;
        }
        for _ in 0..NEXTS_PER_SEEK {
            let Some(entry) = cursor.next().expect("next") else { break };
            black_box(entry);
            count += 1;
        }
    }
    count
}

fn walk_trie<C: TrieCursor>(mut cursor: C) -> usize {
    let mut count = usize::from(cursor.seek(Nibbles::default()).expect("seek").is_some());
    while count < WALK_LEN {
        let Some(entry) = cursor.next().expect("next") else { break };
        black_box(entry);
        count += 1;
    }
    count
}

fn seek_next_trie<C: TrieCursor>(mut cursor: C, targets: &[Nibbles]) -> usize {
    let mut count = 0;
    for target in targets {
        if cursor.seek(*target).expect("seek").is_none() {
            continue;
        }
        for _ in 0..NEXTS_PER_SEEK {
            let Some(entry) = cursor.next().expect("next") else { break };
            black_box(entry);
            count += 1;
        }
    }
    count
}

fn cursor_iteration_benches(c: &mut Criterion) {
    for (layout, recent_in_memtable) in [("compacted", false), ("memtable_overlay", true)] {
        let fixture = create_fixture(recent_in_memtable);
        let store = &fixture.storage;
        let mut rng = StdRng::seed_from_u64(11);
        let hashed_targets: Vec<B256> = (0..SEEKS).map(|_| random_hash(&mut rng)).collect();
        let path_targets = random_paths(&mut rng, SEEKS);

        let mut group = c.benchmark_group(format!("cursor_iteration/{layout}"));
        group.sample_size(20);

        for (bound, max_block) in [("latest", BLOCKS), ("historical", BLOCKS / 2)] {
            let id = |name: &str| BenchmarkId::new(name, bound);
            group.bench_function(id("account_next_1000"), |b| {
                b.iter(|| walk_hashed(store.account_hashed_cursor(max_block).expect("cursor")))
            });
            group.bench_function(id("storage_next_1000"), |b| {
                b.iter(|| {
                    walk_hashed(
                        store
                            .storage_hashed_cursor(fixture.storage_address, max_block)
                            .expect("cursor"),
                    )
                })
            });
            group.bench_function(id("account_trie_next_1000"), |b| {
                b.iter(|| walk_trie(store.account_trie_cursor(max_block).expect("cursor")))
            });
            group.bench_function(id("storage_trie_next_1000"), |b| {
                b.iter(|| {
                    walk_trie(
                        store
                            .storage_trie_cursor(fixture.storage_address, max_block)
                            .expect("cursor"),
                    )
                })
            });
            group.bench_function(id("account_seek_next16_x64"), |b| {
                b.iter(|| {
                    seek_next_hashed(
                        store.account_hashed_cursor(max_block).expect("cursor"),
                        &hashed_targets,
                    )
                })
            });
            group.bench_function(id("storage_seek_next16_x64"), |b| {
                b.iter(|| {
                    seek_next_hashed(
                        store
                            .storage_hashed_cursor(fixture.storage_address, max_block)
                            .expect("cursor"),
                        &hashed_targets,
                    )
                })
            });
            group.bench_function(id("account_trie_seek_next16_x64"), |b| {
                b.iter(|| {
                    seek_next_trie(
                        store.account_trie_cursor(max_block).expect("cursor"),
                        &path_targets,
                    )
                })
            });
        }
        group.finish();
    }
}

criterion_group!(benches, cursor_iteration_benches);
criterion_main!(benches);
