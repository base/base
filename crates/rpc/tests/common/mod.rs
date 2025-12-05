#![allow(dead_code)]

use std::sync::Arc;

use alloy_genesis::Genesis;
use alloy_primitives::{B256, Bytes, b256, bytes};
use reth::api::{NodeTypes, NodeTypesWithDBAdapter};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::{ERROR_DB_CREATION, TempDatabase, create_test_static_files_dir, tempdir_path},
};
use reth_provider::{ProviderFactory, providers::StaticFileProvider};
use serde_json;

pub(crate) const BLOCK_INFO_TXN: Bytes = bytes!(
    "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000"
);
pub(crate) const BLOCK_INFO_TXN_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");

pub(crate) fn create_provider_factory<N: NodeTypes>(
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

pub(crate) fn load_genesis() -> Genesis {
    serde_json::from_str(include_str!("genesis.json")).unwrap()
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
