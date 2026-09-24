//! Benchmarks a cursor `next()` walk (`RocksdbVersionedCursor::next_live_candidate`)
//! across a keyspace dominated by TOMBSTONED keys that each carry a deep
//! version chain, sorting before a small number of genuinely live keys.
//!
//! Every graveyard key is written across `VERSIONS_PER_DEAD_KEY` sequential
//! blocks and then tombstoned in one final block. A forward cursor walk must
//! resolve each graveyard key as dead and skip past its entire version chain
//! before reaching the live keys. This isolates the fix under discussion: a
//! resolved-dead key's remaining raw rows should be skipped with a single
//! seek instead of being scanned one row at a time.

use std::{hint::black_box, sync::Arc};

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{B256, U256};
use base_execution_trie::{BaseProofsInitialStateStore, BaseProofsStore, RocksdbProofsStorage};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_primitives_traits::Account;
use reth_trie::hashed_cursor::HashedCursor;
use reth_trie_common::{HashedPostState, updates::TrieUpdates};
use tempfile::TempDir;

const GRAVEYARD_KEYS: usize = 200;
const VERSIONS_PER_DEAD_KEY: u64 = 200;
const LIVE_KEYS: usize = 8;

struct TombstoneFixture {
    _dir: TempDir,
    storage: Arc<RocksdbProofsStorage>,
    max_block_number: u64,
}

fn key_for(index: usize) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&(index as u64).to_be_bytes());
    B256::from(bytes)
}

fn graveyard_key(index: usize) -> B256 {
    key_for(index)
}

fn live_key(index: usize) -> B256 {
    key_for(GRAVEYARD_KEYS + 1_000_000 + index)
}

fn account_at(nonce: u64) -> Account {
    Account { nonce, balance: U256::from(nonce), bytecode_hash: None }
}

const fn block_for(number: u64, parent: B256) -> BlockWithParent {
    BlockWithParent {
        parent,
        block: BlockNumHash {
            number,
            hash: if number == 0 { B256::ZERO } else { B256::repeat_byte(number as u8) },
        },
    }
}

fn store_accounts(
    rocksdb: &Arc<RocksdbProofsStorage>,
    block_ref: BlockWithParent,
    entries: impl IntoIterator<Item = (B256, Option<Account>)>,
) {
    let mut post_state = HashedPostState::default();
    for (key, value) in entries {
        post_state.accounts.insert(key, value);
    }
    rocksdb
        .store_trie_updates(
            block_ref,
            base_execution_trie::BlockStateDiff {
                sorted_trie_updates: TrieUpdates::default().into_sorted(),
                sorted_post_state: post_state.into_sorted(),
            },
        )
        .expect("store block state diff");
}

fn create_fixture() -> TombstoneFixture {
    let dir = TempDir::new().expect("create temp dir");
    let rocksdb = Arc::new(RocksdbProofsStorage::new(dir.path()).expect("create RocksDB storage"));

    let genesis = (0..GRAVEYARD_KEYS)
        .map(|index| (graveyard_key(index), Some(account_at(0))))
        .chain((0..LIVE_KEYS).map(|index| (live_key(index), Some(account_at(0)))));
    rocksdb.store_hashed_accounts(genesis.collect()).expect("store genesis hashed accounts");
    rocksdb.set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO)).expect("set anchor");
    rocksdb.commit_initial_state().expect("commit initial state");

    let mut parent_hash = B256::ZERO;
    for version in 1..=VERSIONS_PER_DEAD_KEY {
        let block_ref = block_for(version, parent_hash);
        store_accounts(
            &rocksdb,
            block_ref,
            (0..GRAVEYARD_KEYS).map(|index| (graveyard_key(index), Some(account_at(version)))),
        );
        parent_hash = block_ref.block.hash;
    }

    let tombstone_block = VERSIONS_PER_DEAD_KEY + 1;
    let block_ref = block_for(tombstone_block, parent_hash);
    store_accounts(
        &rocksdb,
        block_ref,
        (0..GRAVEYARD_KEYS).map(|index| (graveyard_key(index), None)),
    );

    let fixture =
        TombstoneFixture { _dir: dir, storage: rocksdb, max_block_number: tombstone_block };
    validate_fixture(&fixture);
    fixture
}

fn validate_fixture(fixture: &TombstoneFixture) {
    let mut cursor =
        fixture.storage.account_hashed_cursor(fixture.max_block_number).expect("open cursor");
    let mut live_count = 0;
    while let Some((key, _account)) = cursor.next().expect("cursor next") {
        assert!(key >= live_key(0), "graveyard key surfaced as live: {key}");
        live_count += 1;
    }
    assert_eq!(live_count, LIVE_KEYS, "expected only the live keys to survive the walk");
}

fn walk_full_keyspace(fixture: &TombstoneFixture) -> usize {
    let mut cursor =
        fixture.storage.account_hashed_cursor(fixture.max_block_number).expect("open cursor");
    let mut count = 0;
    while let Some(entry) = cursor.next().expect("cursor next") {
        black_box(entry);
        count += 1;
    }
    count
}

fn tombstone_skip_benches(c: &mut Criterion) {
    let fixture = create_fixture();

    let mut group = c.benchmark_group("tombstone_skip_reads");
    group.sample_size(10);

    group.bench_function(
        BenchmarkId::new(
            "full_keyspace_walk",
            format!("{GRAVEYARD_KEYS}_dead_x_{VERSIONS_PER_DEAD_KEY}_versions"),
        ),
        |b| {
            b.iter(|| black_box(walk_full_keyspace(&fixture)));
        },
    );

    group.finish();
}

criterion_group!(benches, tombstone_skip_benches);
criterion_main!(benches);
