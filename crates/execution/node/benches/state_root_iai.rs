//! Deterministic instruction-count benchmark for one block's state root over the node database.
//!
//! Measures the synchronous state root a Base node computes for a block's changes:
//! [`StateProviderFactory::state_by_block_hash`] for the parent on the production
//! [`BlockchainProvider`] for [`BaseNode`], then [`StateRootProvider::state_root_with_updates`]
//! with the block's [`HashedPostState`]. The Flashblocks builder calls exactly this at block
//! finalization, and the native payload builder falls back to it when its parallel state-root job
//! fails. It runs once per block, ~4×10^5 blocks/day after the Denim network upgrade, and must
//! fit in the tail of the 200 ms slot.
//!
//! The database holds the Base mainnet genesis allocation (2,064 predeploy contracts) plus
//! [`BASE_ACCOUNTS`] seeded accounts in its hashed state tables: [`BASE_CONTRACTS`] contracts
//! with [`SLOTS_PER_CONTRACT`] slots each and the rest externally owned, together with the
//! account and storage trie tables for that state, written the way a block's changes are
//! persisted. The block's changes are
//! [`UPDATED_EOAS`] updated and [`NEW_EOAS`] new externally owned accounts, and
//! [`TOUCHED_CONTRACTS`] contracts that each update [`UPDATED_SLOTS`], clear [`CLEARED_SLOTS`],
//! and add [`NEW_SLOTS`] slots: 384 changed accounts and 3,072 changed slots. Keys are keccak
//! hashes of their index, so they spread over the trie like real hashed keys and every run builds
//! the same tries. The root runs over both Reth storage layouts: `v1` (legacy trie key encoding)
//! and `v2`, the Base default ([`StorageSettings::base`], packed trie key encoding). The bench
//! builds with the crate's default features, so `keccak256` is the portable implementation;
//! production binaries enable `asm-keccak` and `keccak-cache-global`, so the hashing share is
//! higher here than in production.
//!
//! Seeding the database and its tries, and one warm-up computation that pays lazy
//! initialization, run in iai's unmeasured setup; the computation is read-only, so the warm-up
//! does not change the counted one. The block's changes are moved into the counted call, which
//! consumes them as production does, and the fixture and trie updates are handed back so they
//! drop outside the counted region. The computation is single-threaded, and callgrind only counts
//! the thread that enters the benchmark function, so MDBX's transaction manager thread is not
//! counted.
//!
//! The teardown pins the computed root and the number of account and storage trie updates. They
//! are fixed by the workload, so a mismatch means the state root path now computes something
//! different, and the instruction count is not comparable with the base commit's. Setup checks
//! the seeded root and the pinned root against roots computed from scratch in memory, without
//! the database or its tries.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::{hint::black_box, sync::Arc};

use alloy_primitives::{B256, U256, b256, keccak256};
use base_execution_chainspec::BaseChainSpec;
use base_node_core::BaseNode;
use iai_callgrind::{
    Callgrind, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
use reth_chainspec::EthChainSpec;
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_db_api::models::StorageSettings;
use reth_db_common::init::init_genesis_with_settings;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_primitives_traits::Account;
use reth_provider::{
    BlockHashReader, DBProvider, DatabaseProviderFactory, StateProviderFactory, StateRootProvider,
    StateWriter, StorageSettingsCache, TrieWriter, providers::BlockchainProvider,
    test_utils::create_test_provider_factory_with_node_types,
};
use reth_trie::{
    EMPTY_ROOT_HASH, HashedPostState, HashedStorage,
    root::{state_root_unsorted, storage_root_unsorted},
    updates::TrieUpdates,
};

/// Externally owned accounts in the base state.
const BASE_EOAS: usize = 30_720;
/// Contracts in the base state, keyed after the externally owned accounts.
const BASE_CONTRACTS: usize = 2_048;
/// Accounts in the base state.
const BASE_ACCOUNTS: usize = BASE_EOAS + BASE_CONTRACTS;
/// Storage slots per base-state contract.
const SLOTS_PER_CONTRACT: usize = 8;
/// Base-state externally owned accounts the block updates.
const UPDATED_EOAS: usize = 256;
/// Externally owned accounts the block creates, keyed after the base state.
const NEW_EOAS: usize = 32;
/// Base-state contracts whose storage the block changes.
const TOUCHED_CONTRACTS: usize = 96;
/// Existing slots each touched contract updates.
const UPDATED_SLOTS: usize = 6;
/// Existing slots each touched contract clears.
const CLEARED_SLOTS: usize = SLOTS_PER_CONTRACT - UPDATED_SLOTS;
/// Slots each touched contract adds.
const NEW_SLOTS: usize = 24;

/// Pinned state root after the block's changes.
const EXPECTED_ROOT: B256 =
    b256!("0x62e0782a50ce45781dffd86a218e9ec4feb605c02e27ec1800443c4397da2fce");

/// The node binaries run on jemalloc, which also serves MDBX's and `RocksDB`'s C allocations
/// (`override_allocator_on_supported_platforms`). Using it here keeps those allocations
/// production-like and the count stable: under glibc malloc, the free lists that MDBX's cursor
/// `calloc` draws from depend on frees by the database's background threads, which moved the
/// sibling `state_reads_iai` count by up to 5% between runs.
#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Production Base provider over a temporary MDBX database.
type BaseProvider =
    BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, Arc<TempDatabase<DatabaseEnv>>>>;

/// Counts of the trie updates a state root computation returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdateCounts {
    account_nodes: usize,
    removed_account_nodes: usize,
    storage_tries: usize,
    storage_nodes: usize,
    removed_storage_nodes: usize,
}

impl From<&TrieUpdates> for UpdateCounts {
    fn from(updates: &TrieUpdates) -> Self {
        let storage_tries = updates.storage_tries_ref();
        Self {
            account_nodes: updates.account_nodes_ref().len(),
            removed_account_nodes: updates.removed_nodes_ref().len(),
            storage_tries: storage_tries.len(),
            storage_nodes: storage_tries.values().map(|trie| trie.storage_nodes_ref().len()).sum(),
            removed_storage_nodes: storage_tries
                .values()
                .map(|trie| trie.removed_nodes_ref().len())
                .sum(),
        }
    }
}

/// A seeded database at the parent of the block whose root is computed.
struct StateRootFixture {
    provider: BaseProvider,
    parent_hash: B256,
}

/// The fixture plus the block's changes, which the counted call consumes.
type Input = (StateRootFixture, HashedPostState);

impl StateRootFixture {
    /// Seeds the base state and its tries on top of the Base mainnet genesis, checks the pinned
    /// root against an in-memory model, then warms up with one computation of the block.
    fn setup(settings: StorageSettings) -> Input {
        let chain_spec = Arc::new(BaseChainSpec::mainnet());
        let genesis_state = Self::genesis_state(&chain_spec);
        let factory = create_test_provider_factory_with_node_types::<BaseNode>(chain_spec);
        init_genesis_with_settings(&factory, settings).expect("genesis should initialize");
        factory.set_storage_settings_cache(settings);

        // Persist the base state the way a block's changes are: its root and trie updates
        // computed over the genesis tries, then the hashed state and tries in one transaction.
        let base_state = Self::base_state();
        let (base_root, trie) = factory
            .latest()
            .expect("latest state should open")
            .state_root_with_updates(base_state.clone())
            .expect("base state root should compute");
        let provider_rw = factory.database_provider_rw().expect("write provider should open");
        provider_rw
            .write_hashed_state(&base_state.clone().into_sorted())
            .expect("base state should persist");
        provider_rw.write_trie_updates(trie).expect("base tries should persist");
        let parent_hash = provider_rw
            .block_hash(0)
            .expect("genesis hash lookup should succeed")
            .expect("genesis hash should exist");
        provider_rw.commit().expect("base state should commit");

        let mut model = genesis_state;
        model.extend(base_state);
        assert_eq!(base_root, in_memory_root(&model), "seeded root matches the model");
        model.extend(Self::block_changes());
        assert_eq!(in_memory_root(&model), EXPECTED_ROOT, "pinned root matches the model");

        let fixture = Self {
            provider: BlockchainProvider::new(factory).expect("blockchain provider should open"),
            parent_hash,
        };
        let (root, updates) = fixture.state_root(HashedPostState::default());
        assert_eq!(
            (root, UpdateCounts::from(&updates)),
            (
                base_root,
                UpdateCounts {
                    account_nodes: 16,
                    removed_account_nodes: 16,
                    storage_tries: 0,
                    storage_nodes: 0,
                    removed_storage_nodes: 0,
                }
            ),
            "the seeded tries are complete: an empty change set only revisits the top level"
        );
        let (root, updates) = fixture.state_root(Self::block_changes());
        check_outcome(root, &updates);
        (fixture, Self::block_changes())
    }

    /// The Base mainnet genesis allocation (the predeploys) as hashed state.
    fn genesis_state(chain_spec: &BaseChainSpec) -> HashedPostState {
        let mut state = HashedPostState::default();
        for (address, account) in &chain_spec.genesis().alloc {
            let key = keccak256(address);
            state.accounts.insert(key, Some(Account::from(account)));
            let storage = account.storage.iter().flatten();
            state.storages.insert(
                key,
                HashedStorage::from_iter(
                    storage.map(|(slot, value)| (keccak256(slot), U256::from_be_bytes(value.0))),
                ),
            );
        }
        state
    }

    /// Computes the block's state root the way block assembly does.
    fn state_root(&self, hashed_state: HashedPostState) -> (B256, TrieUpdates) {
        self.provider
            .state_by_block_hash(self.parent_hash)
            .expect("parent state should open")
            .state_root_with_updates(hashed_state)
            .expect("state root should compute")
    }

    fn base_state() -> HashedPostState {
        let mut state = HashedPostState::default();
        for index in 0..BASE_EOAS {
            state.accounts.insert(account_key(index), Some(eoa(index, 0)));
        }
        for index in BASE_EOAS..BASE_ACCOUNTS {
            state.accounts.insert(account_key(index), Some(contract(index)));
            let storage =
                (0..SLOTS_PER_CONTRACT).map(|slot| (slot_key(index, slot), value(index, slot, 0)));
            state.storages.insert(account_key(index), HashedStorage::from_iter(storage));
        }
        state
    }

    fn block_changes() -> HashedPostState {
        let mut state = HashedPostState::default();
        for index in (0..BASE_EOAS).step_by(BASE_EOAS / UPDATED_EOAS) {
            state.accounts.insert(account_key(index), Some(eoa(index, 1)));
        }
        for index in BASE_ACCOUNTS..BASE_ACCOUNTS + NEW_EOAS {
            state.accounts.insert(account_key(index), Some(eoa(index, 0)));
        }
        for index in (BASE_EOAS..BASE_ACCOUNTS)
            .step_by(BASE_CONTRACTS / TOUCHED_CONTRACTS)
            .take(TOUCHED_CONTRACTS)
        {
            state.accounts.insert(account_key(index), Some(contract(index)));
            let updated =
                (0..UPDATED_SLOTS).map(|slot| (slot_key(index, slot), value(index, slot, 1)));
            let cleared =
                (UPDATED_SLOTS..SLOTS_PER_CONTRACT).map(|slot| (slot_key(index, slot), U256::ZERO));
            let added = (SLOTS_PER_CONTRACT..SLOTS_PER_CONTRACT + NEW_SLOTS)
                .map(|slot| (slot_key(index, slot), value(index, slot, 1)));
            state.storages.insert(
                account_key(index),
                HashedStorage::from_iter(updated.chain(cleared).chain(added)),
            );
        }
        assert_eq!(state.accounts.len(), UPDATED_EOAS + NEW_EOAS + TOUCHED_CONTRACTS);
        assert_eq!(
            state.storages.values().map(|storage| storage.storage.len()).sum::<usize>(),
            TOUCHED_CONTRACTS * (UPDATED_SLOTS + CLEARED_SLOTS + NEW_SLOTS)
        );
        state
    }
}

/// Root of `state`, computed from scratch in memory without the database or its tries.
fn in_memory_root(state: &HashedPostState) -> B256 {
    state_root_unsorted(state.accounts.iter().filter_map(|(&key, account)| {
        let storage = state.storages.get(&key).map_or(EMPTY_ROOT_HASH, |storage| {
            storage_root_unsorted(
                storage.storage.iter().filter(|(_, value)| !value.is_zero()).map(|(&k, &v)| (k, v)),
            )
        });
        account.map(|account| (key, account.into_trie_account(storage)))
    }))
}

/// Hashed address of account `index`.
fn account_key(index: usize) -> B256 {
    keccak256((index as u64).to_be_bytes())
}

/// Hashed key of `slot` in the storage of account `index`.
fn slot_key(index: usize, slot: usize) -> B256 {
    let mut preimage = [0; 16];
    preimage[..8].copy_from_slice(&(index as u64).to_be_bytes());
    preimage[8..].copy_from_slice(&(slot as u64).to_be_bytes());
    keccak256(preimage)
}

/// Externally owned account `index` after `transactions` transactions.
fn eoa(index: usize, transactions: u64) -> Account {
    Account {
        nonce: index as u64 % 97 + transactions,
        balance: U256::from((index as u64 + 1) * 1_000_000_000 - transactions * 21_000),
        bytecode_hash: None,
    }
}

/// Contract account `index`.
fn contract(index: usize) -> Account {
    Account {
        nonce: 1,
        balance: U256::ZERO,
        bytecode_hash: Some(keccak256((u64::MAX - index as u64).to_be_bytes())),
    }
}

/// Nonzero value of `slot` in account `index` at `version`.
fn value(index: usize, slot: usize, version: u64) -> U256 {
    U256::from((index * 64 + slot) as u64 + 1) + U256::from(version) * U256::from(u64::MAX)
}

/// Checks a computation of the block against the pinned root and update counts.
fn check_outcome(root: B256, updates: &TrieUpdates) {
    assert_eq!(
        (root, UpdateCounts::from(updates)),
        (
            EXPECTED_ROOT,
            UpdateCounts {
                account_nodes: 552,
                removed_account_nodes: 549,
                storage_tries: 76,
                storage_nodes: 120,
                removed_storage_nodes: 7,
            }
        ),
        "state root or trie update counts changed"
    );
}

/// Checks the counted computation against the pinned outcome.
fn check_root(((root, updates), _): ((B256, TrieUpdates), StateRootFixture)) {
    check_outcome(root, &updates);
}

#[library_benchmark]
#[bench::v1(args = (StorageSettings::v1()), setup = StateRootFixture::setup, teardown = check_root)]
#[bench::v2(args = (StorageSettings::v2()), setup = StateRootFixture::setup, teardown = check_root)]
fn block_root((fixture, hashed_state): Input) -> ((B256, TrieUpdates), StateRootFixture) {
    (black_box(fixture.state_root(hashed_state)), fixture)
}

library_benchmark_group!(
    name = state_root;
    benchmarks = block_root
);

// Cache simulation roughly triples valgrind's cost for the whole process, including the
// database seeding in setup, and only Instructions is compared, so it is off to keep the run
// within its time budget. jemalloc's thread-cache garbage collection runs every 64 KiB the thread
// allocates, so where it lands in the counted region depends on how much setup allocated, which
// varies with the database's background threads and moved the sibling `state_reads_iai` count
// by up to 0.8%; raising the interval to 1 TiB keeps it out of the run. Its time-based purging of
// unused pages moved this count by 0.1% when it fired inside the run, so it is disabled.
main!(
    config = LibraryBenchmarkConfig::default()
        .tool(Callgrind::with_args(["--cache-sim=no"]))
        .env(
            "MALLOC_CONF",
            "tcache_gc_incr_bytes:1099511627776,dirty_decay_ms:-1,muzzy_decay_ms:-1",
        );
    library_benchmark_groups = state_root
);
