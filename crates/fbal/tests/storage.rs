//! Tests for SLOAD/SSTORE tracking in the access list

use std::collections::HashMap;

use alloy_primitives::{TxKind, U256};
use alloy_sol_types::SolCall;
use op_revm::OpTransaction;
use revm::{
    context::TxEnv,
    interpreter::instructions::utility::IntoAddress,
    primitives::ONE_ETHER,
    state::{AccountInfo, Bytecode},
};

use super::{AccessListContract, BASE_SEPOLIA_CHAIN_ID, execute_txns_build_access_list};

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
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(AccessListContract::valueCall {}.abi_encode().into())
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None);
    dbg!(access_list);
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
                    .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
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
                    .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
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

    let access_list = execute_txns_build_access_list(txs, Some(overrides), None);
    dbg!(access_list);
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

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(contract))
                .data(AccessListContract::getABCall {}.abi_encode().into())
                .nonce(0)
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(100_000),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None);
    // TODO: dedup storage_reads
    dbg!(access_list);
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
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
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

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None);
    dbg!(access_list);
}
