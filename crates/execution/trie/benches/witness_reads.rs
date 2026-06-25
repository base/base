//! Benchmarks the proofs-history read path used by debug witness generation.
//!
//! This benchmark seeds a `RocksDB` proofs-history store with deterministic
//! account and storage data, then repeatedly performs the DB-bound reads that
//! `debug_executionWitness` drives through `StateProviderDatabase` and
//! `revm::State` while EVM execution records touched state.

use std::{hint::black_box, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, U256, keccak256};
use base_execution_trie::{
    BaseProofsInitialStateStore, BaseProofsStorage, BaseProofsStore, RocksdbProofsStorage,
    provider::BaseProofsStateProviderRef,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand_08::{RngCore, SeedableRng, rngs::StdRng};
use reth_primitives_traits::Account;
use reth_provider::{AccountReader, StateProofProvider, StateProvider, noop::NoopProvider};
use reth_revm::{
    Database, State, database::StateProviderDatabase, witness::ExecutionWitnessRecord,
};
use reth_trie_common::{ExecutionWitnessMode, TrieInput};
use tempfile::TempDir;

const BASE_ACCOUNTS: usize = 10_000;
const SLOTS_PER_ACCOUNT: usize = 8;
const TARGET_ACCOUNTS: usize = 256;
const MISSING_ACCOUNTS: usize = 64;

#[derive(Clone)]
struct SeedAccount {
    address: Address,
    hashed_address: B256,
    account: Account,
    slots: Vec<(B256, B256, U256)>,
}

struct WitnessReadFixture {
    _dir: TempDir,
    storage: BaseProofsStorage<Arc<RocksdbProofsStorage>>,
    targets: Vec<SeedAccount>,
    missing_addresses: Vec<Address>,
}

fn random_bytes<const N: usize>(rng: &mut StdRng) -> [u8; N] {
    let mut bytes = [0; N];
    rng.fill_bytes(&mut bytes);
    bytes
}

fn account_for(index: usize) -> Account {
    Account {
        nonce: index as u64,
        balance: U256::from((index as u64 + 1) * 1_000),
        bytecode_hash: None,
    }
}

fn generate_accounts(count: usize, slots_per_account: usize) -> Vec<SeedAccount> {
    let mut rng = StdRng::seed_from_u64(0xBEEF_F00D);
    (0..count)
        .map(|index| {
            let address = Address::from(random_bytes::<20>(&mut rng));
            let hashed_address = keccak256(address);
            let account = account_for(index);
            let slots = (0..slots_per_account)
                .map(|slot_index| {
                    let storage_key = B256::from(random_bytes::<32>(&mut rng));
                    let hashed_storage_key = keccak256(storage_key);
                    let value = U256::from((index as u64 + 1) * 10_000 + slot_index as u64);
                    (storage_key, hashed_storage_key, value)
                })
                .collect();
            SeedAccount { address, hashed_address, account, slots }
        })
        .collect()
}

fn missing_addresses() -> Vec<Address> {
    (0..MISSING_ACCOUNTS).map(|index| Address::repeat_byte(0x80 | index as u8)).collect()
}

fn create_fixture(compact: bool) -> WitnessReadFixture {
    let dir = TempDir::new().expect("create temp dir");
    let rocksdb = Arc::new(RocksdbProofsStorage::new(dir.path()).expect("create RocksDB storage"));
    let accounts = generate_accounts(BASE_ACCOUNTS, SLOTS_PER_ACCOUNT);

    rocksdb
        .store_hashed_accounts(
            accounts
                .iter()
                .map(|account| (account.hashed_address, Some(account.account)))
                .collect(),
        )
        .expect("store hashed accounts");

    for account in &accounts {
        let storages = account
            .slots
            .iter()
            .map(|(_, hashed_storage_key, value)| (*hashed_storage_key, *value));
        rocksdb
            .store_hashed_storages(account.hashed_address, storages.collect())
            .expect("store hashed storage");
    }

    rocksdb
        .set_initial_state_anchor(BlockNumHash::new(0, B256::ZERO))
        .expect("set initial state anchor");
    rocksdb.commit_initial_state().expect("commit initial state");

    if compact {
        rocksdb.flush_and_compact().expect("flush and compact RocksDB fixture");
    }

    let targets = accounts.into_iter().take(TARGET_ACCOUNTS).collect::<Vec<_>>();
    let storage = BaseProofsStorage::from(rocksdb);
    let fixture =
        WitnessReadFixture { _dir: dir, storage, targets, missing_addresses: missing_addresses() };
    validate_fixture(&fixture);
    fixture
}

fn validate_fixture(fixture: &WitnessReadFixture) {
    let provider =
        BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &fixture.storage, 0);

    for target in &fixture.targets {
        let account =
            provider.basic_account(&target.address).expect("read account").expect("account exists");
        assert_eq!(account, target.account);

        for (storage_key, _, value) in &target.slots {
            let storage_value = provider
                .storage(target.address, *storage_key)
                .expect("read storage")
                .expect("storage exists");
            assert_eq!(storage_value, *value);
        }

        assert!(
            provider
                .storage(target.address, B256::repeat_byte(0xFE))
                .expect("read missing storage")
                .is_none()
        );
    }

    for missing_address in &fixture.missing_addresses {
        assert!(provider.basic_account(missing_address).expect("read missing account").is_none());
    }
}

fn read_accounts_and_storage_with_provider<Storage>(
    provider: &BaseProofsStateProviderRef<'_, Storage>,
    fixture: &WitnessReadFixture,
) -> usize
where
    Storage: BaseProofsStore + Clone,
{
    let mut state = State::builder()
        .with_database(StateProviderDatabase::new(provider))
        .with_bundle_update()
        .build();
    read_accounts_and_storage_with_state(&mut state, fixture)
}

fn read_accounts_and_storage_with_state<DB>(
    state: &mut State<StateProviderDatabase<DB>>,
    fixture: &WitnessReadFixture,
) -> usize
where
    State<StateProviderDatabase<DB>>: reth_revm::Database,
{
    let mut reads = 0;

    for target in &fixture.targets {
        black_box(state.basic(target.address).expect("read account").expect("account exists"));
        reads += 1;

        for (storage_key, _, _) in &target.slots {
            black_box(state.storage(target.address, (*storage_key).into()).expect("read storage"));
            reads += 1;
        }

        black_box(
            state
                .storage(target.address, B256::repeat_byte(0xFE).into())
                .expect("read missing storage"),
        );
        reads += 1;
    }

    for missing_address in &fixture.missing_addresses {
        black_box(state.basic(*missing_address).expect("read missing account"));
        reads += 1;
    }

    reads
}

fn read_accounts_and_storage(fixture: &WitnessReadFixture) -> usize {
    let provider =
        BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &fixture.storage, 0);
    read_accounts_and_storage_with_provider(&provider, fixture)
}

fn read_accounts_storage_and_witness(fixture: &WitnessReadFixture) -> usize {
    let provider =
        BaseProofsStateProviderRef::new(Box::<NoopProvider>::default(), &fixture.storage, 0);
    let mut state = State::builder()
        .with_database(StateProviderDatabase::new(&provider))
        .with_bundle_update()
        .build();
    let reads = read_accounts_and_storage_with_state(&mut state, fixture);
    let ExecutionWitnessRecord { hashed_state, codes, keys, lowest_block_number } =
        ExecutionWitnessRecord::from_executed_state(&state, ExecutionWitnessMode::default());
    black_box(codes);
    black_box(keys);
    black_box(lowest_block_number);
    black_box(
        provider
            .witness(TrieInput::default(), hashed_state, ExecutionWitnessMode::default())
            .expect("build witness"),
    );
    reads
}

fn witness_read_benches(c: &mut Criterion) {
    let fixture = create_fixture(false);
    let compacted_fixture = create_fixture(true);
    let mut group = c.benchmark_group("debug_execution_witness/proofs_history_reads");
    group.sample_size(10);

    group.bench_function(BenchmarkId::new("account_storage_reads", TARGET_ACCOUNTS), |b| {
        b.iter(|| black_box(read_accounts_and_storage(&fixture)));
    });

    group.bench_function(BenchmarkId::new("reads_plus_witness_overlay", TARGET_ACCOUNTS), |b| {
        b.iter(|| black_box(read_accounts_storage_and_witness(&fixture)));
    });

    group.bench_function(
        BenchmarkId::new("account_storage_reads_compacted", TARGET_ACCOUNTS),
        |b| {
            b.iter(|| black_box(read_accounts_and_storage(&compacted_fixture)));
        },
    );

    group.bench_function(
        BenchmarkId::new("reads_plus_witness_overlay_compacted", TARGET_ACCOUNTS),
        |b| {
            b.iter(|| black_box(read_accounts_storage_and_witness(&compacted_fixture)));
        },
    );

    group.finish();
}

criterion_group!(benches, witness_read_benches);
criterion_main!(benches);
