//! Tests for SLOAD/SSTORE tracking in the access list

use std::collections::HashMap;

use super::{
    AccessListContract, AccountInfo, Bytecode, DEVNET_CHAIN_ID, IntoAddress, ONE_ETHER,
    OpTransaction, SolCall, TxEnv, TxKind, U256, execute_txns_build_access_list,
};

#[test]
/// Tests that we can SLOAD a zero-value from a freshly deployed contract's state
fn test_sload_zero_value() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(DEVNET_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(AccessListContract::valueCall {}.abi_encode().into())
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify contract is in the access list
    let contract_entry = access_list.account_changes.iter().find(|ac| ac.address == contract);
    assert!(contract_entry.is_some(), "Contract should be in access list");

    // Verify storage read is recorded (slot 0 for `value`)
    let contract_changes = contract_entry.unwrap();
    let slot_0 = U256::ZERO;
    let has_storage_read = contract_changes.storage_reads.iter().any(|sr| *sr == slot_0);
    assert!(has_storage_read, "Contract should have storage read for slot 0 (value)");
}

#[test]
/// Tests that we can SSTORE and later SLOAD one value from a contract's state
fn test_update_one_value() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let mut txs = Vec::new();
    txs.push(
        OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(sender)
                    .chain_id(Some(DEVNET_CHAIN_ID))
                    .kind(TxKind::Call(contract))
                    .data(
                        AccessListContract::updateValueCall { newValue: U256::from(42) }
                            .abi_encode()
                            .into(),
                    )
                    .nonce(0)
                    .gas_price(0)
                    .gas_priority_fee(None)
                    .max_fee_per_gas(0)
                    .gas_limit(100_000),
            )
            .build_fill(),
    );
    txs.push(
        OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(sender)
                    .chain_id(Some(DEVNET_CHAIN_ID))
                    .kind(TxKind::Call(contract))
                    .data(AccessListContract::valueCall {}.abi_encode().into())
                    .nonce(1)
                    .gas_price(0)
                    .gas_priority_fee(None)
                    .max_fee_per_gas(0)
                    .gas_limit(100_000),
            )
            .build_fill(),
    );

    let access_list = execute_txns_build_access_list(txs, Some(overrides), None)
        .expect("access list build should succeed");

    // Verify contract is in the access list
    let contract_entry = access_list.account_changes.iter().find(|ac| ac.address == contract);
    assert!(contract_entry.is_some(), "Contract should be in access list");

    let contract_changes = contract_entry.unwrap();

    // Verify storage write at slot 0 with new value 42 at tx_index 0
    let slot_0 = U256::ZERO;
    let storage_change = contract_changes.storage_changes.iter().find(|sc| sc.slot == slot_0);
    assert!(storage_change.is_some(), "Contract should have storage change for slot 0");

    let slot_change = storage_change.unwrap();
    assert!(
        slot_change.changes.iter().any(|c| c.block_access_index == 0),
        "Storage change should be at tx_index 0"
    );
    assert!(
        slot_change.changes.iter().any(|c| c.new_value == U256::from(42)),
        "Storage value should be 42"
    );

    // Verify storage read is recorded
    let has_storage_read = contract_changes.storage_reads.iter().any(|sr| *sr == slot_0);
    assert!(has_storage_read, "Contract should have storage read for slot 0");
}

#[test]
/// Ensures that storage reads that read the same slot multiple times are deduped properly
fn test_multi_sload_same_slot() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    // getAb reads both `a` and `b` which are packed in slot 1
    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(DEVNET_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(AccessListContract::getAbCall {}.abi_encode().into())
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify contract is in the access list
    let contract_entry = access_list.account_changes.iter().find(|ac| ac.address == contract);
    assert!(contract_entry.is_some(), "Contract should be in access list");

    let contract_changes = contract_entry.unwrap();

    // Verify storage reads exist - `a` and `b` are packed in slot 1
    // The slot should only appear once even if read multiple times
    let slot_1 = U256::from(1);
    let slot_1_reads: Vec<_> =
        contract_changes.storage_reads.iter().filter(|sr| **sr == slot_1).collect();
    assert_eq!(slot_1_reads.len(), 1, "Slot 1 should only appear once in storage_reads (deduped)");
}

#[test]
/// Ensures that storage writes that update multiple slots are recorded properly
fn test_multi_sstore() {
    let sender = U256::from(0xDEAD).into_address();
    let contract = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        contract,
        AccountInfo::default()
            .with_code(Bytecode::new_raw(AccessListContract::DEPLOYED_BYTECODE.clone())),
    );

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(DEVNET_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(
                    AccessListContract::insertMultipleCall {
                        keys: vec![U256::from(0), U256::from(1)],
                        values: vec![U256::from(84), U256::from(53)],
                    }
                    .abi_encode()
                    .into(),
                )
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify contract is in the access list
    let contract_entry = access_list.account_changes.iter().find(|ac| ac.address == contract);
    assert!(contract_entry.is_some(), "Contract should be in access list");

    let contract_changes = contract_entry.unwrap();

    // Verify we have storage changes for the mapping slots
    // The mapping `data` is at slot 3, so keys hash to keccak256(key . slot)
    assert!(
        contract_changes.storage_changes.len() >= 2,
        "Contract should have at least 2 storage changes for the mapping writes"
    );
}
