//! Genesis configuration utilities for testing.

use std::collections::BTreeMap;

use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256, utils::parse_ether};

use super::Account;

/// Chain ID for devnet test network.
pub const DEVNET_CHAIN_ID: u64 = 84538453;

/// Gas limit for genesis block configuration.
pub const GENESIS_GAS_LIMIT: u64 = 100_000_000;

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
        chain_id: DEVNET_CHAIN_ID,
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
        gas_limit: GENESIS_GAS_LIMIT,
        base_fee_per_gas: Some(1_000_000_000),
        difficulty: U256::ZERO,
        nonce: 0,
        timestamp: 1,
        extra_data: Bytes::from_static(&[0x00]),
        mix_hash: B256::ZERO,
        coinbase: Address::ZERO,
        ..Default::default()
    }
}
