//! Benchmarks proofs-history point reads whose hash-order neighbors are dead keys.
//!
//! `BaseProofsStateProviderRef::basic_account` and `storage` used to answer
//! point reads with a range-cursor `seek()` plus an equality check. On a miss,
//! the `RocksDB` cursor walks forward to the next live key, iterating every
//! version row of each dead neighbor on the way (a key whose latest version is
//! a tombstone, or whose versions are all newer than the read snapshot), and
//! the storage cursor additionally steps over consecutive zero-valued slots.
//! The provider now uses exact point lookups instead.
//!
//! Each scenario places the dead neighbors immediately after the target in
//! hash order (target hash + 1, + 2, ...) and a live terminator key after
//! them, then measures:
//!
//! - `old`: the previous logic, `seek(target)` on a hashed cursor plus an equality check.
//! - `new`: the provider read path (`basic_account` / `storage`).
//!
//! Scenarios, parameterized over `N` dead versions or zero slots:
//!
//! - `account_tombstone_neighbor`: account miss whose next key is a deleted account with `N`
//!   versions ending in a tombstone.
//! - `account_newer_neighbor`: account miss at block 0 whose next key only has `N` versions newer
//!   than block 0.
//! - `storage_newer_neighbor`: storage miss at block 0 whose next slot only has `N` versions newer
//!   than block 0.
//! - `storage_zero_run`: storage read of a zero-valued slot followed by `N` more zero-valued slots,
//!   then a live slot.

use std::{hint::black_box, sync::Arc};

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{Address, B256, U256, keccak256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, BaseProofsStore, BlockStateDiff,
    RocksdbProofsStorage, provider::BaseProofsStateProviderRef,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reth_primitives_traits::Account;
use reth_provider::{AccountReader, StateProvider, noop::NoopProvider};
use reth_trie::hashed_cursor::HashedCursor;
use reth_trie_common::{HashedPostState, HashedStorage, updates::TrieUpdates};
use tempfile::TempDir;

const DEAD_COUNTS: [u64; 3] = [10, 1_000, 10_000];
const LIVE_VALUE: U256 = U256::from_limbs([1, 0, 0, 0]);

type Storage = BaseProofsStorage<Arc<RocksdbProofsStorage>>;

/// A target key plus the hashed keys that follow it in hash order.
struct Target {
    /// Plain key handed to the provider, which hashes it: a storage slot, or a left-padded
    /// address.
    key: B256,
    /// Hashed key the old cursor path seeks: `keccak256(slot)` or `keccak256(address)`.
    hashed: B256,
}

impl Target {
    fn account(seed: &str) -> Self {
        let address = Address::from_word(keccak256(seed.as_bytes()));
        Self { key: address.into_word(), hashed: keccak256(address) }
    }

    fn slot(seed: &str) -> Self {
        let key = keccak256(seed.as_bytes());
        Self { key, hashed: keccak256(key) }
    }

    fn address(&self) -> Address {
        Address::from_word(self.key)
    }

    /// Hashed key `offset` positions after the target in hash order.
    fn next(&self, offset: u64) -> B256 {
        B256::from(U256::from_be_bytes(self.hashed.0) + U256::from(offset))
    }
}

struct Fixture {
    _dir: TempDir,
    storage: Storage,
    head: u64,
    dead: u64,
    /// Account miss whose neighbor is tombstoned at head.
    tombstone: Target,
    /// Account miss whose neighbor only has versions above block 0.
    newer_account: Target,
    /// Account holding `newer_slot` and its neighbor.
    newer_storage_account: Target,
    /// Storage miss whose neighbor slot only has versions above block 0.
    newer_slot: Target,
    /// Account holding the zero-valued slot run.
    zero_account: Target,
    /// Zero-valued slot followed by `dead` more zero-valued slots.
    zero_slot: Target,
}

const fn account(nonce: u64) -> Account {
    Account { nonce, balance: U256::from_limbs([nonce + 1, 0, 0, 0]), bytecode_hash: None }
}

fn block_hash(number: u64) -> B256 {
    if number == 0 { B256::ZERO } else { B256::from(U256::from(number)) }
}

fn block_for(number: u64) -> BlockWithParent {
    BlockWithParent {
        parent: block_hash(number - 1),
        block: BlockNumHash { number, hash: block_hash(number) },
    }
}

fn create_fixture(dead: u64) -> Fixture {
    let dir = TempDir::new().expect("create temp dir");
    let rocksdb = Arc::new(RocksdbProofsStorage::new(dir.path()).expect("create RocksDB storage"));

    let tombstone = Target::account("tombstone");
    let newer_account = Target::account("newer-account");
    let newer_storage_account = Target::account("newer-storage-account");
    let newer_slot = Target::slot("newer-slot");
    let zero_account = Target::account("zero-account");
    let zero_slot = Target::slot("zero-slot");

    // Block 0: live terminators after each dead run, plus the storage owners.
    rocksdb
        .store_hashed_accounts(vec![
            (tombstone.next(2), Some(account(0))),
            (newer_account.next(2), Some(account(0))),
            (newer_storage_account.hashed, Some(account(0))),
            (zero_account.hashed, Some(account(0))),
        ])
        .expect("store hashed accounts");
    let zero_run = (0..=dead).map(|offset| (zero_slot.next(offset), U256::ZERO));
    rocksdb
        .store_hashed_storages_bulk(vec![
            (newer_storage_account.hashed, vec![(newer_slot.next(2), LIVE_VALUE)]),
            (
                zero_account.hashed,
                zero_run.chain([(zero_slot.next(dead + 1), LIVE_VALUE)]).collect(),
            ),
        ])
        .expect("store hashed storages");
    rocksdb
        .set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))
        .expect("set initial state anchor");
    rocksdb.commit_initial_state().expect("commit initial state");

    // Blocks 1..=dead: version rows for each dead neighbor.
    for number in 1..=dead {
        let mut post_state = HashedPostState::default();
        let tombstone_value = (number < dead).then(|| account(number));
        post_state.accounts.insert(tombstone.next(1), tombstone_value);
        post_state.accounts.insert(newer_account.next(1), Some(account(number)));
        post_state.storages.insert(
            newer_storage_account.hashed,
            HashedStorage::from_iter([(newer_slot.next(1), U256::from(number))]),
        );
        let diff = BlockStateDiff {
            sorted_trie_updates: TrieUpdates::default().into_sorted(),
            sorted_post_state: post_state.into_sorted(),
        };
        rocksdb.store_trie_updates(block_for(number), diff).expect("store block");
    }
    rocksdb.flush_and_compact().expect("flush and compact RocksDB fixture");

    let fixture = Fixture {
        _dir: dir,
        storage: BaseProofsStorage::from(rocksdb),
        head: dead,
        dead,
        tombstone,
        newer_account,
        newer_storage_account,
        newer_slot,
        zero_account,
        zero_slot,
    };
    validate_fixture(&fixture);
    fixture
}

/// Previous `basic_account` logic: hashed cursor `seek()` plus an equality check.
fn old_account(storage: &Storage, hashed: B256, max_block: u64) -> Option<Account> {
    old_account_seek(storage, hashed, max_block)
        .and_then(|(key, value)| (key == hashed).then_some(value))
}

fn old_account_seek(storage: &Storage, hashed: B256, max_block: u64) -> Option<(B256, Account)> {
    let tx = storage.ro_tx().expect("ro tx");
    let mut cursor = storage.account_hashed_cursor_with_tx(&tx, max_block).expect("cursor");
    cursor.seek(hashed).expect("seek")
}

/// Previous `storage` logic: storage cursor `seek()` plus an equality check.
fn old_storage(storage: &Storage, account: B256, slot: B256, max_block: u64) -> Option<U256> {
    old_storage_seek(storage, account, slot, max_block)
        .and_then(|(key, value)| (key == slot).then_some(value))
}

fn old_storage_seek(
    storage: &Storage,
    account: B256,
    slot: B256,
    max_block: u64,
) -> Option<(B256, U256)> {
    let tx = storage.ro_tx().expect("ro tx");
    let mut cursor =
        storage.storage_hashed_cursor_with_tx(&tx, account, max_block).expect("cursor");
    cursor.seek(slot).expect("seek")
}

fn new_account(storage: &Storage, address: Address, max_block: u64) -> Option<Account> {
    BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), storage, max_block)
        .basic_account(&address)
        .expect("read account")
}

fn new_storage(storage: &Storage, address: Address, slot: B256, max_block: u64) -> Option<U256> {
    BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), storage, max_block)
        .storage(address, slot)
        .expect("read storage")
}

fn validate_fixture(f: &Fixture) {
    let storage = &f.storage;

    // Every target reads as absent on both paths.
    assert_eq!(old_account(storage, f.tombstone.hashed, f.head), None);
    assert_eq!(new_account(storage, f.tombstone.address(), f.head), None);
    assert_eq!(old_account(storage, f.newer_account.hashed, 0), None);
    assert_eq!(new_account(storage, f.newer_account.address(), 0), None);
    assert_eq!(old_storage(storage, f.newer_storage_account.hashed, f.newer_slot.hashed, 0), None);
    assert_eq!(new_storage(storage, f.newer_storage_account.address(), f.newer_slot.key, 0), None);
    assert_eq!(old_storage(storage, f.zero_account.hashed, f.zero_slot.hashed, f.head), None);
    assert_eq!(new_storage(storage, f.zero_account.address(), f.zero_slot.key, f.head), None);

    // The old seek walks past every dead neighbor and lands on the live terminator.
    assert_eq!(
        old_account_seek(storage, f.tombstone.hashed, f.head),
        Some((f.tombstone.next(2), account(0)))
    );
    assert_eq!(
        old_account_seek(storage, f.newer_account.hashed, 0),
        Some((f.newer_account.next(2), account(0)))
    );
    assert_eq!(
        old_storage_seek(storage, f.newer_storage_account.hashed, f.newer_slot.hashed, 0),
        Some((f.newer_slot.next(2), LIVE_VALUE))
    );
    assert_eq!(
        old_storage_seek(storage, f.zero_account.hashed, f.zero_slot.hashed, f.head),
        Some((f.zero_slot.next(f.dead + 1), LIVE_VALUE))
    );

    // The dead neighbors are what the fixture claims they are.
    assert_eq!(old_account(storage, f.tombstone.next(1), f.head - 1), Some(account(f.head - 1)));
    assert_eq!(old_account(storage, f.tombstone.next(1), f.head), None);
    assert_eq!(old_account(storage, f.newer_account.next(1), 1), Some(account(1)));
    assert_eq!(
        old_storage(storage, f.newer_storage_account.hashed, f.newer_slot.next(1), 1),
        Some(U256::from(1))
    );
}

fn dead_key_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("dead_key_reads");
    group.sample_size(10);

    for dead in DEAD_COUNTS {
        let f = create_fixture(dead);
        let storage = &f.storage;

        group.bench_function(BenchmarkId::new("account_tombstone_neighbor/old", dead), |b| {
            b.iter(|| black_box(old_account(storage, f.tombstone.hashed, f.head)));
        });
        group.bench_function(BenchmarkId::new("account_tombstone_neighbor/new", dead), |b| {
            b.iter(|| black_box(new_account(storage, f.tombstone.address(), f.head)));
        });

        group.bench_function(BenchmarkId::new("account_newer_neighbor/old", dead), |b| {
            b.iter(|| black_box(old_account(storage, f.newer_account.hashed, 0)));
        });
        group.bench_function(BenchmarkId::new("account_newer_neighbor/new", dead), |b| {
            b.iter(|| black_box(new_account(storage, f.newer_account.address(), 0)));
        });

        group.bench_function(BenchmarkId::new("storage_newer_neighbor/old", dead), |b| {
            b.iter(|| {
                black_box(old_storage(
                    storage,
                    f.newer_storage_account.hashed,
                    f.newer_slot.hashed,
                    0,
                ))
            });
        });
        group.bench_function(BenchmarkId::new("storage_newer_neighbor/new", dead), |b| {
            b.iter(|| {
                black_box(new_storage(
                    storage,
                    f.newer_storage_account.address(),
                    f.newer_slot.key,
                    0,
                ))
            });
        });

        group.bench_function(BenchmarkId::new("storage_zero_run/old", dead), |b| {
            b.iter(|| {
                black_box(old_storage(storage, f.zero_account.hashed, f.zero_slot.hashed, f.head))
            });
        });
        group.bench_function(BenchmarkId::new("storage_zero_run/new", dead), |b| {
            b.iter(|| {
                black_box(new_storage(storage, f.zero_account.address(), f.zero_slot.key, f.head))
            });
        });
    }

    group.finish();
}

criterion_group!(benches, dead_key_benches);
criterion_main!(benches);
