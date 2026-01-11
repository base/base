//! Shared fixtures and test data reused by integration tests across the Base codebase.

use std::sync::Arc;

use alloy_genesis::GenesisAccount;
use alloy_primitives::U256;
use base_primitives::{Account, build_test_genesis};
use reth::api::{NodeTypes, NodeTypesWithDBAdapter};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::{ERROR_DB_CREATION, TempDatabase, create_test_static_files_dir, tempdir_path},
};
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};

/// Creates a test chain spec with pre-funded test accounts.
pub fn load_chain_spec() -> Arc<OpChainSpec> {
    let genesis = build_test_genesis()
        .extend_accounts(
            Account::all()
                .into_iter()
                .map(|a| {
                    (
                        a.address(),
                        GenesisAccount::default().with_balance(U256::from(1_000_000_000_u64)),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .with_gas_limit(100_000_000);

    Arc::new(OpChainSpec::from_genesis(genesis))
}

/// Creates a provider factory for tests with the given chain spec.
pub fn create_provider_factory<N: NodeTypes>(
    chain_spec: Arc<N::ChainSpec>,
) -> ProviderFactory<NodeTypesWithDBAdapter<N, Arc<TempDatabase<DatabaseEnv>>>> {
    let (static_dir, _) = create_test_static_files_dir();
    let db = create_test_db();
    ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
    )
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
