//! Genesis configuration utilities for testing.

use std::collections::BTreeMap;

use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256, utils::parse_ether};
use base_common_consensus::Predeploys;
use base_common_evm::BaseTime;

use crate::Account;

/// Chain ID for devnet test network.
pub const DEVNET_CHAIN_ID: u64 = 84538453;

/// Gas limit for genesis block configuration.
pub const GENESIS_GAS_LIMIT: u64 = 100_000_000;

/// Builds a test genesis configuration programmatically.
///
/// Creates a Base Sepolia-like genesis with:
/// - All EVM and inherited rollup upgrades enabled from genesis
/// - Base EIP-1559 settings (elasticity=6, denominator=50)
/// - Pre-funded test accounts from the `Account` enum
pub fn build_test_genesis() -> Genesis {
    // Base EIP-1559 base fee parameters.
    const EIP1559_ELASTICITY: u64 = 6;
    const EIP1559_DENOMINATOR: u64 = 50;

    // Test account balance: 1 million ETH
    let test_account_balance: U256 = parse_ether("1000000").expect("valid ether amount");

    // Build chain config with all upgrades enabled at genesis
    let config = ChainConfig {
        chain_id: DEVNET_CHAIN_ID,
        // Block-based EVM upgrades (all at block 0)
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
        // Time-based upgrades
        shanghai_time: Some(0),
        cancun_time: Some(0),
        prague_time: Some(0),
        // Post-merge settings
        terminal_total_difficulty: Some(U256::ZERO),
        terminal_total_difficulty_passed: true,
        // Rollup upgrades and settings via extra_fields
        extra_fields: [
            ("bedrockBlock", serde_json::json!(0)),
            ("regolithTime", serde_json::json!(0)),
            ("canyonTime", serde_json::json!(0)),
            ("ecotoneTime", serde_json::json!(0)),
            ("fjordTime", serde_json::json!(0)),
            ("graniteTime", serde_json::json!(0)),
            ("holoceneTime", serde_json::json!(0)),
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
    let mut alloc: BTreeMap<Address, GenesisAccount> = Account::all()
        .into_iter()
        .map(|account| {
            (account.address(), GenesisAccount::default().with_balance(test_account_balance))
        })
        .collect();
    alloc.insert(
        Predeploys::BASE_TIME,
        GenesisAccount::default().with_code(Some(BaseTime::proxy_bytecode())).with_storage(Some(
            BTreeMap::from([
                (
                    B256::from(BaseTime::ADMIN_SLOT.to_be_bytes::<32>()),
                    Predeploys::PROXY_ADMIN.into_word(),
                ),
                (
                    B256::from(BaseTime::IMPLEMENTATION_SLOT.to_be_bytes::<32>()),
                    BaseTime::IMPLEMENTATION_ADDRESS.into_word(),
                ),
            ]),
        )),
    );
    alloc.insert(
        BaseTime::IMPLEMENTATION_ADDRESS,
        GenesisAccount::default().with_code(Some(BaseTime::implementation_bytecode())),
    );

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

/// Builds a test genesis with Base Azul (Osaka) enabled at timestamp 0.
///
/// Extends [`build_test_genesis`] with:
/// - `osaka_time` set to 0
/// - Base Azul activation at timestamp 0
pub fn build_test_genesis_azul() -> Genesis {
    let mut genesis = build_test_genesis();
    genesis.config.osaka_time = Some(0);
    genesis.config.extra_fields.insert("base".to_string(), serde_json::json!({ "azul": 0 }));
    genesis
}

/// Builds a test genesis with Base Azul, Beryl, and Cobalt all enabled at
/// timestamp 0.
///
/// Extends [`build_test_genesis_azul`] with:
/// - Base Beryl and Cobalt activation at timestamp 0
/// - `activationAdminAddress` set to the [`Account::Deployer`] address (required
///   once Beryl is active)
pub fn build_test_genesis_cobalt() -> Genesis {
    let mut genesis = build_test_genesis_azul();
    genesis
        .config
        .extra_fields
        .insert("base".to_string(), serde_json::json!({ "azul": 0, "beryl": 0, "cobalt": 0 }));
    genesis.config.extra_fields.insert(
        "activationAdminAddress".to_string(),
        serde_json::json!(Account::Deployer.address()),
    );
    genesis
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    #[test]
    fn test_genesis_contains_linked_base_time_proxy() {
        let genesis = build_test_genesis();
        let proxy = &genesis.alloc[&Predeploys::BASE_TIME];
        let storage = proxy.storage.as_ref().unwrap();

        assert_eq!(proxy.code.as_ref(), Some(&BaseTime::proxy_bytecode()));
        assert_eq!(
            storage[&B256::from(BaseTime::ADMIN_SLOT.to_be_bytes::<32>())],
            Predeploys::PROXY_ADMIN.into_word()
        );
        assert_eq!(
            storage[&B256::from(BaseTime::IMPLEMENTATION_SLOT.to_be_bytes::<32>())],
            BaseTime::IMPLEMENTATION_ADDRESS.into_word()
        );

        let implementation = &genesis.alloc[&BaseTime::IMPLEMENTATION_ADDRESS];
        assert_eq!(
            implementation.code.as_ref().map(keccak256),
            Some(BaseTime::IMPLEMENTATION_CODE_HASH)
        );
    }
}
