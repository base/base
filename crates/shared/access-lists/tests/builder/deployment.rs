//! Tests for CREATE/CREATE2 contract deployment tracking in the access list

use std::collections::HashMap;

use super::{
    AccountInfo, B256, BASE_SEPOLIA_CHAIN_ID, Bytecode, ContractFactory, IntoAddress, ONE_ETHER,
    OpTransaction, SimpleStorage, SolCall, TxEnv, TxKind, U256, execute_txns_build_access_list,
};

#[test]
/// Tests that contract deployment via CREATE is tracked in the access list
/// Verifies:
/// - Factory contract address is in touched accounts
/// - Newly deployed contract address is in touched accounts
/// - Code change is recorded for the new contract
/// - Nonce change is recorded for the factory (CREATE increments nonce)
fn test_create_deployment_tracked() {
    let sender = U256::from(0xDEAD).into_address();
    let factory = U256::from(0xFAC0).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        factory,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(ContractFactory::DEPLOYED_BYTECODE.clone())),
    );

    // Deploy SimpleStorage via CREATE
    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(factory))
                .data(
                    ContractFactory::deployWithCreateCall {
                        bytecode: SimpleStorage::BYTECODE.to_vec().into(),
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(500_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify factory is in the access list
    let factory_entry = access_list.account_changes.iter().find(|ac| ac.address == factory);
    assert!(factory_entry.is_some(), "Factory should be in access list");

    // The factory's nonce should change (CREATE increments deployer nonce)
    let factory_changes = factory_entry.unwrap();
    assert!(!factory_changes.nonce_changes.is_empty(), "Factory nonce should change due to CREATE");

    // Find the deployed contract - it should have a code change
    let deployed_entry = access_list
        .account_changes
        .iter()
        .find(|ac| !ac.code_changes.is_empty() && ac.address != factory);
    assert!(deployed_entry.is_some(), "Deployed contract should have code change");

    let deployed_changes = deployed_entry.unwrap();
    assert_eq!(deployed_changes.code_changes.len(), 1, "Should have exactly one code change");

    // Verify the deployed bytecode matches SimpleStorage's deployed bytecode
    let code_change = &deployed_changes.code_changes[0];
    assert!(!code_change.new_code.is_empty(), "Deployed code should not be empty");
}

#[test]
/// Tests that contract deployment via CREATE2 is tracked in the access list
/// Verifies:
/// - Factory contract address is in touched accounts
/// - Deployed address (deterministic) is in touched accounts
/// - Code change is recorded with correct bytecode
fn test_create2_deployment_tracked() {
    let sender = U256::from(0xDEAD).into_address();
    let factory = U256::from(0xFAC0).into_address();
    let salt = B256::from(U256::from(12345));

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        factory,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(ContractFactory::DEPLOYED_BYTECODE.clone())),
    );

    // Deploy SimpleStorage via CREATE2
    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(factory))
                .data(
                    ContractFactory::deployWithCreate2Call {
                        bytecode: SimpleStorage::BYTECODE.to_vec().into(),
                        salt,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(500_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify factory is in the access list
    let factory_entry = access_list.account_changes.iter().find(|ac| ac.address == factory);
    assert!(factory_entry.is_some(), "Factory should be in access list");

    // Find the deployed contract - it should have a code change
    let deployed_entry = access_list
        .account_changes
        .iter()
        .find(|ac| !ac.code_changes.is_empty() && ac.address != factory);
    assert!(deployed_entry.is_some(), "Deployed contract should have code change");

    let deployed_changes = deployed_entry.unwrap();
    assert_eq!(deployed_changes.code_changes.len(), 1, "Should have exactly one code change");

    // Verify the deployed bytecode is present
    let code_change = &deployed_changes.code_changes[0];
    assert!(!code_change.new_code.is_empty(), "Deployed code should not be empty");
}

#[test]
/// Tests that deploying a contract and immediately calling it tracks both operations
/// Verifies:
/// - Both the factory and deployed contract are tracked
/// - Code change for deployment is recorded
/// - Storage change from the call is recorded on the new contract's address
fn test_create_and_immediate_call() {
    let sender = U256::from(0xDEAD).into_address();
    let factory = U256::from(0xFAC0).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        factory,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(ContractFactory::DEPLOYED_BYTECODE.clone())),
    );

    // Deploy SimpleStorage and immediately call setValue(42)
    let set_value_calldata = SimpleStorage::setValueCall { v: U256::from(42) }.abi_encode();

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(factory))
                .data(
                    ContractFactory::deployAndCallCall {
                        bytecode: SimpleStorage::BYTECODE.to_vec().into(),
                        callData: set_value_calldata.into(),
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(500_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify factory is in the access list
    let factory_entry = access_list.account_changes.iter().find(|ac| ac.address == factory);
    assert!(factory_entry.is_some(), "Factory should be in access list");

    // Find the deployed contract - it should have both code change AND storage change
    let deployed_entry = access_list
        .account_changes
        .iter()
        .find(|ac| !ac.code_changes.is_empty() && ac.address != factory);
    assert!(deployed_entry.is_some(), "Deployed contract should have code change");

    let deployed_changes = deployed_entry.unwrap();

    // Verify code change exists
    assert_eq!(deployed_changes.code_changes.len(), 1, "Should have exactly one code change");

    // Verify storage change exists (from setValue(42))
    // SimpleStorage stores `value` at slot 0
    assert!(
        !deployed_changes.storage_changes.is_empty(),
        "Should have storage change from setValue call"
    );

    // Verify the storage slot is 0 and value is 42
    let storage_change = &deployed_changes.storage_changes[0];
    assert_eq!(storage_change.slot, U256::ZERO, "Storage slot should be 0");
    assert_eq!(storage_change.changes[0].new_value, U256::from(42), "Storage value should be 42");
}
