//! Benchmarks the proofs-history read path against DEEP per-key version chains.
//!
//! Seeds a [`RocksDB`] proofs-history store with `BASE_ACCOUNTS` accounts, each
//! one updated `VERSIONS_PER_KEY` times across sequential blocks, so every
//! `HashedAccountHistory` user-key has a long version chain. Then drives
//! `TARGET_ACCOUNTS` reads through the same `StateProviderDatabase` path that
//! block execution uses, at two read snapshots: chain head (latest version
//! sits at the start of the chain) and chain midpoint (the cursor must skip
//! over the newer half of the chain before landing on its answer).
//!
//! This is where the complement-key encoding's asymptotic win should show: the
//! reversed encoding turns the "latest version <= N" lookup into a single
//! `seek()` instead of a `seek_for_prev()` that on the LSM-tree memtable runs
//! 7-8x slower (`RocksDB` PR #5535).

use std::{hint::black_box, sync::Arc};

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{Address, B256, U256, keccak256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, BaseProofsStore, BlockStateDiff,
    RocksdbProofsStorage, provider::BaseProofsStateProviderRef,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand_08::{RngCore, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_provider::{AccountReader, noop::NoopProvider};
use reth_revm::{Database, State, database::StateProviderDatabase};
use reth_trie_common::{HashedPostState, updates::TrieUpdates};
use tempfile::TempDir;

const BASE_ACCOUNTS: usize = 1_000;
const VERSIONS_PER_KEY: u64 = 100;
const TARGET_ACCOUNTS: usize = 256;
const MISSING_ACCOUNTS: usize = 64;

#[derive(Clone)]
struct SeedAccount {
    address: Address,
    hashed_address: B256,
}

struct DeepHistoryFixture {
    _dir: TempDir,
    storage: BaseProofsStorage<Arc<RocksdbProofsStorage>>,
    accounts: Vec<SeedAccount>,
    targets: Vec<SeedAccount>,
    missing_addresses: Vec<Address>,
}

fn random_bytes<const N: usize>(rng: &mut StdRng) -> [u8; N] {
    let mut bytes = [0; N];
    rng.fill_bytes(&mut bytes);
    bytes
}

fn account_at_version(index: usize, version: u64) -> Account {
    Account {
        nonce: version,
        balance: U256::from((index as u64 + 1) * 1_000 + version),
        bytecode_hash: None,
    }
}

fn generate_accounts(count: usize) -> Vec<SeedAccount> {
    let mut rng = StdRng::seed_from_u64(0xDEAD_BEEF);
    (0..count)
        .map(|_| {
            let address = Address::from(random_bytes::<20>(&mut rng));
            let hashed_address = keccak256(address);
            SeedAccount { address, hashed_address }
        })
        .collect()
}

fn missing_addresses() -> Vec<Address> {
    (0..MISSING_ACCOUNTS).map(|index| Address::repeat_byte(0x80 | index as u8)).collect()
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

fn diff_for_all_accounts(accounts: &[SeedAccount], version: u64) -> BlockStateDiff {
    let mut post_state = HashedPostState::default();
    for (index, account) in accounts.iter().enumerate() {
        post_state
            .accounts
            .insert(account.hashed_address, Some(account_at_version(index, version)));
    }
    BlockStateDiff {
        sorted_trie_updates: TrieUpdates::default().into_sorted(),
        sorted_post_state: post_state.into_sorted(),
    }
}

fn create_fixture(compact: bool) -> DeepHistoryFixture {
    let dir = TempDir::new().expect("create temp dir");
    let rocksdb = Arc::new(RocksdbProofsStorage::new(dir.path()).expect("create RocksDB storage"));
    let accounts = generate_accounts(BASE_ACCOUNTS);

    rocksdb
        .store_hashed_accounts(
            accounts
                .iter()
                .enumerate()
                .map(|(index, account)| {
                    (account.hashed_address, Some(account_at_version(index, 0)))
                })
                .collect(),
        )
        .expect("store hashed accounts");
    rocksdb
        .set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))
        .expect("set initial state anchor");
    rocksdb.commit_initial_state().expect("commit initial state");

    let mut parent_hash = B256::ZERO;
    for version in 1..=VERSIONS_PER_KEY {
        let block_ref = block_for(version, parent_hash);
        let diff = diff_for_all_accounts(&accounts, version);
        rocksdb.store_trie_updates(block_ref, diff).expect("store version");
        parent_hash = block_ref.block.hash;
    }

    if compact {
        rocksdb.flush_and_compact().expect("flush and compact RocksDB fixture");
    }

    let targets = accounts.iter().take(TARGET_ACCOUNTS).cloned().collect::<Vec<_>>();
    let storage = BaseProofsStorage::from(rocksdb);
    let fixture = DeepHistoryFixture {
        _dir: dir,
        storage,
        accounts,
        targets,
        missing_addresses: missing_addresses(),
    };
    validate_fixture(&fixture);
    fixture
}

fn validate_fixture(fixture: &DeepHistoryFixture) {
    let head_provider = BaseProofsStateProviderRef::new(
        Box::<NoopProvider>::default(),
        &fixture.storage,
        VERSIONS_PER_KEY,
    );
    let mid_block = VERSIONS_PER_KEY / 2;
    let mid_provider = BaseProofsStateProviderRef::new(
        Box::<NoopProvider>::default(),
        &fixture.storage,
        mid_block,
    );

    for (index, target) in fixture.accounts.iter().enumerate().take(8) {
        let head =
            head_provider.basic_account(&target.address).expect("head read").expect("head exists");
        assert_eq!(head, account_at_version(index, VERSIONS_PER_KEY));

        let mid =
            mid_provider.basic_account(&target.address).expect("mid read").expect("mid exists");
        assert_eq!(mid, account_at_version(index, mid_block));
    }

    for missing_address in fixture.missing_addresses.iter().take(8) {
        assert!(head_provider.basic_account(missing_address).expect("missing read").is_none());
    }
}

fn read_accounts_at_block(fixture: &DeepHistoryFixture, max_block: u64) -> usize {
    let provider = BaseProofsStateProviderRef::new(
        Box::<NoopProvider>::default(),
        &fixture.storage,
        max_block,
    );
    let mut state = State::builder()
        .with_database(StateProviderDatabase::new(&provider))
        .with_bundle_update()
        .build();

    let mut reads = 0;
    for target in &fixture.targets {
        black_box(state.basic(target.address).expect("read account").expect("account exists"));
        reads += 1;
    }
    for missing_address in &fixture.missing_addresses {
        black_box(state.basic(*missing_address).expect("read missing account"));
        reads += 1;
    }
    reads
}

fn deep_history_benches(c: &mut Criterion) {
    let uncompacted = create_fixture(false);
    let compacted = create_fixture(true);
    let mid_block = VERSIONS_PER_KEY / 2;

    let mut group = c.benchmark_group("deep_history_reads");
    group.sample_size(10);

    group.bench_function(BenchmarkId::new("chain_head_reads", TARGET_ACCOUNTS), |b| {
        b.iter(|| black_box(read_accounts_at_block(&uncompacted, VERSIONS_PER_KEY)));
    });

    group.bench_function(BenchmarkId::new("chain_mid_reads", TARGET_ACCOUNTS), |b| {
        b.iter(|| black_box(read_accounts_at_block(&uncompacted, mid_block)));
    });

    group.bench_function(BenchmarkId::new("chain_head_reads_compacted", TARGET_ACCOUNTS), |b| {
        b.iter(|| black_box(read_accounts_at_block(&compacted, VERSIONS_PER_KEY)));
    });

    group.bench_function(BenchmarkId::new("chain_mid_reads_compacted", TARGET_ACCOUNTS), |b| {
        b.iter(|| black_box(read_accounts_at_block(&compacted, mid_block)));
    });

    group.finish();
}

criterion_group!(benches, deep_history_benches);
criterion_main!(benches);
