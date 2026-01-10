//! Tests for DELEGATECALL storage pattern tracking

use std::collections::HashMap;

use super::{
    AccountInfo, BASE_SEPOLIA_CHAIN_ID, Bytecode, IntoAddress, Logic, Logic2, ONE_ETHER,
    OpTransaction, Proxy, SolCall, TxEnv, TxKind, U256, execute_txns_build_access_list,
};

#[test]
fn test_delegatecall_storage_tracked_on_caller() {
    let sender = U256::from(0xDEAD).into_address();
    let logic_addr = U256::from(0xBEEF).into_address();
    let proxy_addr = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    overrides.insert(
        logic_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Logic::DEPLOYED_BYTECODE.clone())),
    );

    overrides.insert(
        proxy_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Proxy::DEPLOYED_BYTECODE.clone())),
    );

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(proxy_addr))
                .data(Logic::setValueCall { v: U256::from(42) }.abi_encode().into())
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(200_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(
        vec![tx],
        Some(overrides),
        Some(HashMap::from([(proxy_addr, HashMap::from([(U256::ZERO, logic_addr.into_word())]))])),
    )
    .expect("access list build should succeed");

    let proxy_changes = access_list
        .account_changes
        .iter()
        .find(|ac| ac.address == proxy_addr)
        .expect("Proxy should be in account changes");

    let slot_1 = U256::from(1);
    let has_slot_1_change = proxy_changes.storage_changes.iter().any(|sc| sc.slot == slot_1);
    assert!(has_slot_1_change, "Proxy should have storage change for slot 1 (value)");

    let logic_changes = access_list.account_changes.iter().find(|ac| ac.address == logic_addr);
    if let Some(logic) = logic_changes {
        assert!(logic.storage_changes.is_empty(), "Logic contract should have no storage changes");
    }
}

#[test]
fn test_delegatecall_read_tracked_on_caller() {
    let sender = U256::from(0xDEAD).into_address();
    let logic_addr = U256::from(0xBEEF).into_address();
    let proxy_addr = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        logic_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Logic::DEPLOYED_BYTECODE.clone())),
    );
    overrides.insert(
        proxy_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Proxy::DEPLOYED_BYTECODE.clone())),
    );

    let set_tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(proxy_addr))
                .data(Logic::setValueCall { v: U256::from(42) }.abi_encode().into())
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(200_000),
        )
        .build_fill();

    let get_tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(proxy_addr))
                .data(Logic::getValueCall {}.abi_encode().into())
                .nonce(1)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(200_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(
        vec![set_tx, get_tx],
        Some(overrides),
        Some(HashMap::from([(proxy_addr, HashMap::from([(U256::ZERO, logic_addr.into_word())]))])),
    )
    .expect("access list build should succeed");

    let proxy_changes = access_list
        .account_changes
        .iter()
        .find(|ac| ac.address == proxy_addr)
        .expect("Proxy should be in account changes");

    let slot_1 = U256::from(1);
    let has_slot_1_read = proxy_changes.storage_reads.iter().any(|sr| *sr == slot_1);
    assert!(has_slot_1_read, "Proxy should have storage read for slot 1");

    assert!(
        access_list.account_changes.iter().any(|ac| ac.address == proxy_addr),
        "Proxy should be in touched accounts"
    );
    assert!(
        access_list.account_changes.iter().any(|ac| ac.address == logic_addr),
        "Logic should be in touched accounts"
    );
}

#[test]
fn test_delegatecall_chain() {
    let sender = U256::from(0xDEAD).into_address();
    let logic_addr = U256::from(0xBEEF).into_address();
    let logic2_addr = U256::from(0xFACE).into_address();
    let proxy_addr = U256::from(0xCAFE).into_address();

    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));
    overrides.insert(
        logic_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Logic::DEPLOYED_BYTECODE.clone())),
    );
    overrides.insert(
        logic2_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Logic2::DEPLOYED_BYTECODE.clone())),
    );
    overrides.insert(
        proxy_addr,
        AccountInfo::default().with_code(Bytecode::new_raw(Proxy::DEPLOYED_BYTECODE.clone())),
    );

    let storage_overrides = HashMap::from([(
        proxy_addr,
        HashMap::from([
            (U256::ZERO, logic2_addr.into_word()),
            (U256::from(3), logic_addr.into_word()),
        ]),
    )]);

    let inner_call = Logic::setValueCall { v: U256::from(99) }.abi_encode();
    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(proxy_addr))
                .data(
                    Logic2::chainedDelegatecallCall { data: inner_call.into() }.abi_encode().into(),
                )
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(300_000),
        )
        .build_fill();

    let access_list =
        execute_txns_build_access_list(vec![tx], Some(overrides), Some(storage_overrides))
            .expect("access list build should succeed");

    assert!(
        access_list.account_changes.iter().any(|ac| ac.address == proxy_addr),
        "Proxy should be in touched accounts"
    );
    assert!(
        access_list.account_changes.iter().any(|ac| ac.address == logic2_addr),
        "Logic2 should be in touched accounts"
    );
    assert!(
        access_list.account_changes.iter().any(|ac| ac.address == logic_addr),
        "Logic should be in touched accounts"
    );

    let proxy_changes = access_list
        .account_changes
        .iter()
        .find(|ac| ac.address == proxy_addr)
        .expect("Proxy should have account changes");

    let slot_1 = U256::from(1);
    let has_value_change = proxy_changes.storage_changes.iter().any(|sc| sc.slot == slot_1);
    assert!(has_value_change, "Proxy should have storage change for slot 1 (value)");

    if let Some(logic2) = access_list.account_changes.iter().find(|ac| ac.address == logic2_addr) {
        assert!(logic2.storage_changes.is_empty(), "Logic2 should have no storage changes");
    }
    if let Some(logic) = access_list.account_changes.iter().find(|ac| ac.address == logic_addr) {
        assert!(logic.storage_changes.is_empty(), "Logic should have no storage changes");
    }
}
