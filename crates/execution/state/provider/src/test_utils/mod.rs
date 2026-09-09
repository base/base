//! Provider test fixtures and helpers.

use std::sync::Arc;

use alloy_primitives::B256;
use base_common_chain_config::{BaseChainSpec, BaseChainSpecBuilder};
use base_execution_state_database::{
    DatabaseEnv, mdbx::DatabaseArguments, test_utils::TempDatabase,
};
use base_execution_state_memory::StoredAccount as Account;
use base_execution_state_types::ProviderResult;
use base_execution_state_types::StorageEntry;
use reth_trie::{DatabaseStateRoot, StateRoot};

use crate::{
    ChainSpecProvider, HashingWriter, ProviderFactory, TrieWriter,
    providers::{RocksDBBuilder, StaticFileProvider, StaticFileProviderBuilder},
};

type DbStateRoot<'a, TX, A> = StateRoot<
    reth_trie::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie::DatabaseHashedCursorFactory<&'a TX>,
>;

pub mod blocks;
mod mock;
mod noop;

pub use mock::{ExtendedAccount, MockEthProvider};
pub use noop::NoopProvider;
pub use reth_chain_state::test_utils::TestCanonStateSubscriptions;

/// Temporary database retained for the lifetime of provider tests.
pub type MockNodeDatabase = Arc<TempDatabase<DatabaseEnv>>;

/// Creates test provider factory with mainnet chain spec.
pub fn create_test_provider_factory() -> ProviderFactory {
    create_test_provider_factory_with_chain_spec(std::sync::Arc::new(
        base_common_chain_config::BaseChainSpec::mainnet(),
    ))
}

/// Creates test provider factory with provided chain spec.
pub fn create_test_provider_factory_with_chain_spec(
    chain_spec: Arc<BaseChainSpec>,
) -> ProviderFactory {
    let genesis_block_number = chain_spec.genesis.number.unwrap_or_default();
    create_test_provider_factory_with_genesis(chain_spec, genesis_block_number)
}

/// Creates a test provider factory whose chain starts at `genesis_block_number`.
pub fn create_test_provider_factory_with_genesis_block_number(
    genesis_block_number: u64,
) -> ProviderFactory {
    let mut genesis = BaseChainSpec::mainnet().genesis;
    genesis.number = Some(genesis_block_number);
    let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().genesis(genesis).build());
    create_test_provider_factory_with_chain_spec(chain_spec)
}

fn create_test_provider_factory_with_genesis(
    chain_spec: Arc<BaseChainSpec>,
    genesis_block_number: u64,
) -> ProviderFactory {
    // Create a single temp directory that contains all data dirs (db, static_files, rocksdb).
    // TempDatabase will clean up the entire directory on drop.
    let datadir_path = base_execution_state_database::test_utils::tempdir_path();

    let static_files_path = datadir_path.join("static_files");
    let rocksdb_path = datadir_path.join("rocksdb");

    // Create static_files directory
    std::fs::create_dir_all(&static_files_path).expect("failed to create static_files dir");

    // Create database with the datadir path so TempDatabase cleans up everything on drop
    let db =
        base_execution_state_database::test_utils::create_test_rw_db_with_datadir(&datadir_path);

    ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProviderBuilder::read_write(static_files_path)
            .with_genesis_block_number(genesis_block_number)
            .build()
            .expect("static file provider"),
        RocksDBBuilder::new(&rocksdb_path)
            .with_default_tables()
            .build()
            .expect("failed to create test RocksDB provider"),
        base_common_runtime_tasks::Runtime::test(),
    )
    .expect("failed to create test provider factory")
}

/// Creates test provider factory with provided chain spec and custom database arguments.
///
/// Same as [`create_test_provider_factory_with_chain_spec`] but allows overriding the default
/// test database arguments (e.g. to increase the MDBX geometry for heavy benchmarks).
pub fn create_test_provider_factory_with_chain_spec_and_db_args(
    chain_spec: Arc<BaseChainSpec>,
    db_args: DatabaseArguments,
) -> ProviderFactory {
    let datadir_path = base_execution_state_database::test_utils::tempdir_path();

    let db_path = datadir_path.join("db");
    let static_files_path = datadir_path.join("static_files");
    let rocksdb_path = datadir_path.join("rocksdb");

    std::fs::create_dir_all(&static_files_path).expect("failed to create static_files dir");

    let db = base_execution_state_database::init_db(&db_path, db_args).expect("failed to init db");
    let db = Arc::new(TempDatabase::new(db, datadir_path));

    ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProvider::read_write(static_files_path).expect("static file provider"),
        RocksDBBuilder::new(&rocksdb_path)
            .with_default_tables()
            .build()
            .expect("failed to create test RocksDB provider"),
        base_common_runtime_tasks::Runtime::test(),
    )
    .expect("failed to create test provider factory")
}

/// Inserts the provider's genesis allocation into the trie.
pub fn insert_genesis(provider_factory: &ProviderFactory) -> ProviderResult<B256> {
    let provider = provider_factory.provider_rw()?;

    // Hash accounts and insert them into hashing table.
    let chain_spec = provider_factory.chain_spec();
    let genesis = chain_spec.genesis();
    let alloc_accounts =
        genesis.alloc.iter().map(|(addr, account)| (*addr, Some(Account::from(account))));
    provider.insert_account_for_hashing(alloc_accounts).unwrap();

    let alloc_storage = genesis.alloc.clone().into_iter().filter_map(|(addr, account)| {
        // Only return `Some` if there is storage.
        account.storage.map(|storage| {
            (
                addr,
                storage.into_iter().map(|(key, value)| StorageEntry { key, value: value.into() }),
            )
        })
    });
    provider.insert_storage_for_hashing(alloc_storage)?;

    let (root, updates) = {
        type A = reth_trie::PackedKeyAdapter;
        DbStateRoot::<_, A>::from_tx(provider.tx_ref()).root_with_updates()?
    };
    provider.write_trie_updates(updates).unwrap();

    provider.commit()?;

    Ok(root)
}

mod changesets;
pub use changesets::TestChangesets;
