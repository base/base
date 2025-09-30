use std::sync::Arc;

use reth::api::{NodeTypes, NodeTypesWithDBAdapter};
use reth_db::{
    init_db,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration, KILOBYTE, MEGABYTE},
    test_utils::{create_test_static_files_dir, tempdir_path, TempDatabase, ERROR_DB_CREATION},
    ClientVersion, DatabaseEnv,
};

use reth_provider::{providers::StaticFileProvider, ProviderFactory};

pub fn create_test_provider_factory<N: NodeTypes>(
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
