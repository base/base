//! Deterministic instruction-count benchmark for a block's worth of database state reads.
//!
//! Measures the state-read path that block building and block execution go through when the
//! execution cache misses: [`StateProviderFactory::state_by_block_hash`] on the production
//! [`BlockchainProvider`] for [`BaseNode`], composed by [`BuilderStateProvider`] (the stack both
//! Base payload builders read through, here without the execution cache or instrumentation), and
//! read through revm's [`Database`] trait on [`StateProviderDatabase`], as `State` does on a
//! cache miss. Base runs this per block, ~4×10^5 blocks/day after the Denim network upgrade.
//!
//! The database holds the Base mainnet genesis allocation (2,064 predeploy contracts) plus
//! [`ACCOUNT_COUNT`] accounts with [`SLOTS_PER_ACCOUNT`] slots each, created in block 1 and each
//! rewritten once in one of blocks 2 to [`HISTORY_BLOCKS`], so history indices and changesets are
//! populated. One block of reads is [`ACCOUNT_READS`] `basic` lookups (the seeded accounts plus
//! absent ones) and, for each present account, [`PRESENT_SLOT_READS`] seeded slots and one absent
//! slot: 5,376 reads, mixing hits and misses. Absent accounts get no storage reads because revm's
//! `State` answers those from its own cache. The reads run against the latest state (`latest`)
//! and against the state after block [`HISTORICAL_BLOCK`] (`historical`), where some accounts
//! resolve through history indices and changesets and the rest fall through to current state.
//! Each runs over both Reth storage layouts: `v1` keeps everything in MDBX and reads plain state;
//! `v2`, the Base default ([`StorageSettings::base`]), reads hashed state from MDBX and history
//! from `RocksDB` and static files. `v2` hashes the address and slot of every latest-state read.
//! The bench builds with the crate's default features, so `keccak256` is the portable
//! implementation; production binaries enable `asm-keccak` and `keccak-cache-global`, so the
//! keccak share of `v2` is higher here than in production.
//!
//! Seeding and opening the database, flushing and compacting `RocksDB` so no background flush or
//! compaction runs during the measurement, and one warm-up pass that pays lazy initialization
//! (static-file and `RocksDB` handles, block cache, MDBX transaction pool) run in iai's unmeasured
//! setup, and the inputs are handed back so they drop outside the counted region. Callgrind only
//! counts the thread that enters the benchmark function, so the MDBX transaction manager and
//! `RocksDB` threads are not counted. Opening the state provider, the reads, and dropping the
//! provider (which ends its read transaction) are counted, as they run once per block.
//!
//! The teardown pins the number of account and storage hits and misses, and a checksum of every
//! value read, against the seeded model. They are fixed by the workload, so a mismatch means the
//! read path now returns different state, and the instruction count is not comparable with the
//! base commit's.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::{hint::black_box, sync::Arc};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256, keccak256, map::HashMap};
use base_common_consensus::BaseBlock;
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BuilderStateProvider;
use base_node_core::BaseNode;
use iai_callgrind::{
    Callgrind, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
use reth_db::{DatabaseEnv, test_utils::TempDatabase};
use reth_db_api::models::StorageSettings;
use reth_db_common::init::init_genesis_with_settings;
use reth_node_api::NodeTypesWithDBAdapter;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{
    BlockHashReader, BlockWriter, DBProvider, DatabaseProviderFactory, ExecutionOutcome,
    RocksDBProviderFactory, StateProviderFactory, StorageSettingsCache,
    providers::BlockchainProvider, test_utils::create_test_provider_factory_with_node_types,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie::{HashedPostState, KeccakKeyHasher};
use revm::{Database, database::BundleState, state::AccountInfo};

/// Accounts seeded in block 1.
const ACCOUNT_COUNT: usize = 1024;
/// Storage slots seeded per account in block 1.
const SLOTS_PER_ACCOUNT: usize = 4;
/// Blocks on top of genesis. Block 1 creates every account and slot; each later block rewrites
/// the accounts assigned to it by [`modified_at`].
const HISTORY_BLOCKS: u64 = 16;
/// Block whose post-state the `historical` benches read.
const HISTORICAL_BLOCK: u64 = HISTORY_BLOCKS / 2;
/// Accounts looked up per block: every seeded account, then absent ones.
const ACCOUNT_READS: usize = 1280;
/// Seeded slots read per present account, after which one absent slot is read.
const PRESENT_SLOT_READS: usize = 3;

/// The node binaries run on jemalloc, which also serves MDBX's and `RocksDB`'s C allocations
/// (`override_allocator_on_supported_platforms`). Using it here keeps those allocations
/// production-like and the count stable: under glibc malloc, the free lists that MDBX's per-read
/// cursor `calloc` draws from depend on frees by the database's background threads, which moved
/// the count by up to 5% between runs.
#[cfg(unix)]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Production Base provider over a temporary MDBX database.
type BaseProvider =
    BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, Arc<TempDatabase<DatabaseEnv>>>>;

/// Block of the seeded history whose post-state is read.
#[derive(Debug, Clone, Copy)]
enum ReadAt {
    Latest,
    Historical,
}

impl ReadAt {
    const fn block(self) -> u64 {
        match self {
            Self::Latest => HISTORY_BLOCKS,
            Self::Historical => HISTORICAL_BLOCK,
        }
    }
}

/// Outcome of one block of reads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReadTotals {
    account_hits: usize,
    account_misses: usize,
    storage_hits: usize,
    storage_misses: usize,
    /// Wrapping sum of every nonce, balance, and storage value read.
    checksum: U256,
}

/// A seeded database plus the block of reads to run against it.
struct StateReadFixture {
    provider: BaseProvider,
    block_hash: B256,
    addresses: Vec<Address>,
    /// [`PRESENT_SLOT_READS`] seeded slots, then one that is never seeded.
    slots: [U256; PRESENT_SLOT_READS + 1],
    expected: ReadTotals,
}

impl StateReadFixture {
    fn new(settings: StorageSettings, at: ReadAt) -> Self {
        let factory = create_test_provider_factory_with_node_types::<BaseNode>(Arc::new(
            BaseChainSpec::mainnet(),
        ));
        init_genesis_with_settings(&factory, settings).expect("genesis should initialize");
        factory.set_storage_settings_cache(settings);

        let genesis_hash = factory
            .provider()
            .expect("provider should open")
            .block_hash(0)
            .expect("genesis hash lookup should succeed")
            .expect("genesis hash should exist");
        let blocks = Self::blocks(genesis_hash);
        let block_hash = blocks[(at.block() - 1) as usize].hash();
        let bundle = Self::bundle();
        let hashed_state =
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state()).into_sorted();
        let execution_outcome =
            ExecutionOutcome::new(bundle, vec![Vec::new(); HISTORY_BLOCKS as usize], 1, Vec::new());

        let provider_rw = factory.database_provider_rw().expect("write provider should open");
        provider_rw
            .append_blocks_with_state(blocks, &execution_outcome, hashed_state)
            .expect("history fixture should persist");
        provider_rw.commit().expect("history fixture should commit");
        if settings.any_in_rocksdb() {
            factory.rocksdb_provider().flush_and_compact().expect("RocksDB should flush");
        }

        let fixture = Self {
            provider: BlockchainProvider::new(factory).expect("blockchain provider should open"),
            block_hash,
            addresses: (0..ACCOUNT_READS).map(Self::address).collect(),
            slots: std::array::from_fn(|read| {
                Self::slot(if read < PRESENT_SLOT_READS { read } else { SLOTS_PER_ACCOUNT })
            }),
            expected: Self::expected(at.block()),
        };
        assert_eq!(fixture.read_block(), fixture.expected, "warm-up reads match the seeded model");
        fixture
    }

    /// Reads one block's worth of state the way the payload builder composes its provider.
    fn read_block(&self) -> ReadTotals {
        let state = BuilderStateProvider::new(
            self.provider.state_by_block_hash(self.block_hash).expect("state provider should open"),
            None,
            false,
        );
        let mut db = StateProviderDatabase::new(state.provider());
        let mut totals = ReadTotals::default();
        for &address in &self.addresses {
            let Some(account) = db.basic(address).expect("account read should succeed") else {
                totals.account_misses += 1;
                continue;
            };
            totals.account_hits += 1;
            totals.checksum = totals
                .checksum
                .wrapping_add(U256::from(account.nonce))
                .wrapping_add(account.balance);
            for &slot in &self.slots {
                let value = db.storage(address, slot).expect("storage read should succeed");
                if value.is_zero() {
                    totals.storage_misses += 1;
                } else {
                    totals.storage_hits += 1;
                    totals.checksum = totals.checksum.wrapping_add(value);
                }
            }
        }
        totals
    }

    /// Totals the reads must produce at the post-state of `block`, derived from the model.
    fn expected(block: u64) -> ReadTotals {
        let mut totals =
            ReadTotals { account_misses: ACCOUNT_READS - ACCOUNT_COUNT, ..Default::default() };
        for account in 0..ACCOUNT_COUNT {
            let version = u64::from(modified_at(account) <= block);
            let info = Self::account_info(version, account);
            totals.account_hits += 1;
            totals.checksum =
                totals.checksum.wrapping_add(U256::from(info.nonce)).wrapping_add(info.balance);
            for slot in 0..PRESENT_SLOT_READS {
                totals.storage_hits += 1;
                totals.checksum =
                    totals.checksum.wrapping_add(Self::storage_value(version, account, slot));
            }
            totals.storage_misses += 1;
        }
        totals
    }

    fn blocks(mut parent_hash: B256) -> Vec<RecoveredBlock<BaseBlock>> {
        (1..=HISTORY_BLOCKS)
            .map(|number| {
                let block = RecoveredBlock::new_unhashed(
                    BaseBlock {
                        header: Header {
                            parent_hash,
                            number,
                            timestamp: number,
                            ..Default::default()
                        },
                        body: Default::default(),
                    },
                    Vec::new(),
                );
                parent_hash = block.hash();
                block
            })
            .collect()
    }

    /// Final state with one revert set per block: block 1 creates every account and slot at
    /// version 0, and block [`modified_at`] rewrites the account and its slots to version 1.
    fn bundle() -> BundleState {
        type Revert = Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>;

        let state = (0..ACCOUNT_COUNT).map(|account| {
            let storage = (0..SLOTS_PER_ACCOUNT)
                .map(|slot| (Self::slot(slot), (U256::ZERO, Self::storage_value(1, account, slot))))
                .collect::<HashMap<_, _>>();
            (Self::address(account), None, Some(Self::account_info(1, account)), storage)
        });
        let reverts = (1..=HISTORY_BLOCKS).map(|block| {
            (0..ACCOUNT_COUNT)
                .filter(|&account| block == 1 || modified_at(account) == block)
                .map(|account| {
                    let account_revert = if block == 1 {
                        Some(None)
                    } else {
                        Some(Some(Self::account_info(0, account)))
                    };
                    let storage_reverts = (0..SLOTS_PER_ACCOUNT)
                        .map(|slot| {
                            let value = if block == 1 {
                                U256::ZERO
                            } else {
                                Self::storage_value(0, account, slot)
                            };
                            (Self::slot(slot), value)
                        })
                        .collect();
                    (Self::address(account), account_revert, storage_reverts)
                })
                .collect::<Revert>()
        });

        BundleState::new(state, reverts, [])
    }

    /// Address of account `index`, spread over the key space like real addresses. Indexes at or
    /// above [`ACCOUNT_COUNT`] are never seeded.
    fn address(index: usize) -> Address {
        Address::from_slice(&keccak256((index as u64).to_be_bytes())[12..])
    }

    /// Slot `position`. Positions at or above [`SLOTS_PER_ACCOUNT`] are never seeded.
    fn slot(position: usize) -> U256 {
        U256::from_be_bytes(keccak256((position as u64).to_be_bytes()).0)
    }

    fn account_info(version: u64, account: usize) -> AccountInfo {
        AccountInfo {
            nonce: version + 1,
            balance: U256::from((version * ACCOUNT_COUNT as u64 + account as u64 + 1) * 1_000),
            ..Default::default()
        }
    }

    fn storage_value(version: u64, account: usize, slot: usize) -> U256 {
        U256::from(
            (version * ACCOUNT_COUNT as u64 + account as u64) * SLOTS_PER_ACCOUNT as u64
                + slot as u64
                + 1,
        )
    }
}

/// Block in `2..=HISTORY_BLOCKS` that rewrites `account`.
const fn modified_at(account: usize) -> u64 {
    2 + (account as u64) % (HISTORY_BLOCKS - 1)
}

/// Checks the block's reads against the seeded model and the workload's pinned counts.
fn check_reads((totals, fixture): (ReadTotals, StateReadFixture)) {
    assert_eq!(
        (totals.account_hits, totals.account_misses, totals.storage_hits, totals.storage_misses),
        (1024, 256, 3072, 1024),
        "state_reads hit and miss counts changed: {totals:?}"
    );
    assert_eq!(totals, fixture.expected, "reads match the seeded model");
}

#[library_benchmark]
#[bench::v1(
    args = (StorageSettings::v1(), ReadAt::Latest),
    setup = StateReadFixture::new,
    teardown = check_reads
)]
#[bench::v2(
    args = (StorageSettings::v2(), ReadAt::Latest),
    setup = StateReadFixture::new,
    teardown = check_reads
)]
fn latest(fixture: StateReadFixture) -> (ReadTotals, StateReadFixture) {
    (black_box(fixture.read_block()), fixture)
}

#[library_benchmark]
#[bench::v1(
    args = (StorageSettings::v1(), ReadAt::Historical),
    setup = StateReadFixture::new,
    teardown = check_reads
)]
#[bench::v2(
    args = (StorageSettings::v2(), ReadAt::Historical),
    setup = StateReadFixture::new,
    teardown = check_reads
)]
fn historical(fixture: StateReadFixture) -> (ReadTotals, StateReadFixture) {
    (black_box(fixture.read_block()), fixture)
}

library_benchmark_group!(
    name = state_reads;
    benchmarks = latest, historical
);

// Cache simulation roughly triples valgrind's cost for the whole process, including the
// database seeding in setup, and only Instructions is compared, so it is off to keep the run
// within its time budget. jemalloc's thread-cache garbage collection runs every 64 KiB the thread
// allocates, so where it lands in the counted region depends on how much setup allocated, which
// varies with the database's background threads and moved the count by up to 0.8%;
// raising the interval to 1 TiB keeps it out of the run. Its time-based purging of unused pages
// moved the sibling `state_root_iai` count by 0.1% when it fired inside the run, so it is disabled.
main!(
    config = LibraryBenchmarkConfig::default()
        .tool(Callgrind::with_args(["--cache-sim=no"]))
        .env(
            "MALLOC_CONF",
            "tcache_gc_incr_bytes:1099511627776,dirty_decay_ms:-1,muzzy_decay_ms:-1",
        );
    library_benchmark_groups = state_reads
);
