//! Shared fixtures and test data reused by integration tests across the Base codebase.

use std::{collections::BTreeMap, sync::Arc};

use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256, utils::parse_ether};
use reth::api::{NodeTypes, NodeTypesWithDBAdapter};
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::{ERROR_DB_CREATION, TempDatabase, create_test_static_files_dir, tempdir_path},
};
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};

use crate::Account;

/// Builds a test genesis configuration programmatically.
///
/// Creates a Base Sepolia-like genesis with:
/// - All EVM and OP hardforks enabled from genesis
/// - Optimism EIP-1559 settings (elasticity=6, denominator=50)
/// - Pre-funded test accounts from the `Account` enum
pub fn build_test_genesis() -> Genesis {
    // OP EIP-1559 base fee parameters
    const EIP1559_ELASTICITY: u64 = 6;
    const EIP1559_DENOMINATOR: u64 = 50;

    // Test account balance: 1 million ETH
    let test_account_balance: U256 = parse_ether("1000000").expect("valid ether amount");

    // Build chain config with all hardforks enabled at genesis
    let config = ChainConfig {
        chain_id: crate::BASE_CHAIN_ID,
        // Block-based EVM hardforks (all at block 0)
        homestead_block: Some(0),
        eip150_block: Some(0),
        eip155_block: Some(0),
        eip158_block: Some(0),
        byzantium_block: Some(0),
        constantinople_block: Some(0),
        petersburg_block: Some(0),
        istanbul_block: Some(0),
        muir_glacier_block: Some(0),
        berlin_block: Some(0),
        london_block: Some(0),
        arrow_glacier_block: Some(0),
        gray_glacier_block: Some(0),
        merge_netsplit_block: Some(0),
        // Time-based hardforks
        shanghai_time: Some(0),
        cancun_time: Some(0),
        prague_time: Some(0),
        // Post-merge settings
        terminal_total_difficulty: Some(U256::ZERO),
        terminal_total_difficulty_passed: true,
        // OP-specific hardforks and settings via extra_fields
        extra_fields: [
            ("bedrockBlock", serde_json::json!(0)),
            ("regolithTime", serde_json::json!(0)),
            ("canyonTime", serde_json::json!(0)),
            ("ecotoneTime", serde_json::json!(0)),
            ("fjordTime", serde_json::json!(0)),
            ("graniteTime", serde_json::json!(0)),
            ("isthmusTime", serde_json::json!(0)),
            ("jovianTime", serde_json::json!(0)),
            (
                "optimism",
                serde_json::json!({
                    "eip1559Elasticity": EIP1559_ELASTICITY,
                    "eip1559Denominator": EIP1559_DENOMINATOR
                }),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        ..Default::default()
    };

    // Pre-fund all test accounts
    let alloc: BTreeMap<Address, GenesisAccount> = Account::all()
        .into_iter()
        .map(|account| {
            (account.address(), GenesisAccount::default().with_balance(test_account_balance))
        })
        .collect();

    Genesis {
        config,
        alloc,
        gas_limit: 100_000_000,
        difficulty: U256::ZERO,
        nonce: 0,
        timestamp: 0,
        extra_data: Bytes::from_static(&[0x00]),
        mix_hash: B256::ZERO,
        coinbase: Address::ZERO,
        ..Default::default()
    }
}

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
