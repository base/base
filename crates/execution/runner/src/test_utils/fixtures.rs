//! Shared fixtures and test data reused by integration tests across the Base codebase.

use std::sync::Arc;

use alloy_genesis::GenesisAccount;
use alloy_primitives::{U256, utils::Unit};
use base_execution_chainspec::OpChainSpec;
use base_test_utils::{Account, build_test_genesis};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::{
        ERROR_DB_CREATION, TempDatabase, create_test_rocksdb_dir, create_test_static_files_dir,
        tempdir_path,
    },
};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_provider::{
    ProviderFactory,
    providers::{NodeTypesForProvider, RocksDBBuilder, StaticFileProvider},
};

use crate::test_utils::{GENESIS_GAS_LIMIT, TEST_ACCOUNT_BALANCE_ETH};

/// Creates a test chain spec with pre-funded test accounts.
pub fn load_chain_spec() -> Arc<OpChainSpec> {
    let test_account_balance: U256 =
        Unit::ETHER.wei().saturating_mul(U256::from(TEST_ACCOUNT_BALANCE_ETH));

    let genesis = build_test_genesis()
        .extend_accounts(
            Account::all()
                .into_iter()
                .map(|a| {
                    (a.address(), GenesisAccount::default().with_balance(test_account_balance))
                })
                .collect::<Vec<_>>(),
        )
        .with_gas_limit(GENESIS_GAS_LIMIT);

    Arc::new(OpChainSpec::from_genesis(genesis))
}

/// Creates a provider factory for tests with the given chain spec.
pub fn create_provider_factory<N: NodeTypesForProvider>(
    chain_spec: Arc<N::ChainSpec>,
    runtime: reth_tasks::Runtime,
) -> ProviderFactory<NodeTypesWithDBAdapter<N, Arc<TempDatabase<DatabaseEnv>>>> {
    let (static_dir, _) = create_test_static_files_dir();
    let (rocksdb_dir, _) = create_test_rocksdb_dir();
    let db = create_test_db();
    ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
        RocksDBBuilder::new(&rocksdb_dir).with_default_tables().build().expect("rocks db provider"),
        runtime,
    )
    .expect("create provider factory")
}

/// Creates a temporary test database.
fn create_test_db() -> Arc<TempDatabase<DatabaseEnv>> {
    let path = tempdir_path();
    let emsg = format!("{ERROR_DB_CREATION}: {path:?}");

    let db = init_db(
        &path,
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            .with_geometry_max_size(Some(4 * MEGABYTE))
            .with_growth_step(Some(4 * KILOBYTE)),
    )
    .expect(&emsg);

    Arc::new(TempDatabase::new(db, path))
}
