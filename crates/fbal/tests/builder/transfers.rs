//! Tests for ETH transfer tracking in the access list

use std::collections::HashMap;

use alloy_primitives::{TxKind, U256};
use op_revm::OpTransaction;
use revm::{
    context::TxEnv,
    interpreter::instructions::utility::IntoAddress,
    primitives::ONE_ETHER,
    state::AccountInfo,
};

use super::{BASE_SEPOLIA_CHAIN_ID, execute_txns_build_access_list};

#[test]
/// Tests that the system precompiles get included in the access list
fn test_precompiles() {
    let base_tx =
        TxEnv::builder().chain_id(Some(BASE_SEPOLIA_CHAIN_ID)).gas_limit(50_000).gas_price(0);
    let tx = OpTransaction::builder().base(base_tx).build_fill();
    let access_list = execute_txns_build_access_list(vec![tx], None);
    dbg!(access_list);
}

#[test]
/// Tests that a single ETH transfer is included in the access list
fn test_single_transfer() {
    let sender = U256::from(0xDEAD).into_address();
    let recipient = U256::from(0xBEEF).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(recipient))
                .value(U256::from(1_000_000))
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(21_100),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    dbg!(access_list);
}

#[test]
/// Ensures that when gas is paid, the appropriate balance changes are included
/// Sender balance is deducted as (fee paid + value)
/// Fee Vault/Beneficiary address earns fee paid
fn test_gas_included_in_balance_change() {
    let sender = U256::from(0xDEAD).into_address();
    let recipient = U256::from(0xBEEF).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    let tx = OpTransaction::builder()
        .base(
            TxEnv::builder()
                .caller(sender)
                .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                .kind(TxKind::Call(recipient))
                .value(U256::from(1_000_000))
                .gas_price(1000)
                .gas_priority_fee(Some(1_000))
                .max_fee_per_gas(1_000)
                .gas_limit(21_100),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides));
    dbg!(access_list);
}

#[test]
/// Ensures that multiple transfers between the same sender/recipient
/// in a single direction are all processed correctly
fn test_multiple_transfers() {
    let sender = U256::from(0xDEAD).into_address();
    let recipient = U256::from(0xBEEF).into_address();
    let mut overrides = HashMap::new();
    overrides.insert(sender, AccountInfo::from_balance(U256::from(ONE_ETHER)));

    let mut txs = Vec::new();
    for i in 0..10 {
        let tx = OpTransaction::builder()
            .base(
                TxEnv::builder()
                    .caller(sender)
                    .chain_id(Some(BASE_SEPOLIA_CHAIN_ID))
                    .nonce(i)
                    .kind(TxKind::Call(recipient))
                    .value(U256::from(1_000_000))
                    .gas_price(0)
                    .gas_priority_fee(None)
                    .max_fee_per_gas(0)
                    .gas_limit(21_100),
            )
            .build_fill();
        txs.push(tx);
    }

    let access_list = execute_txns_build_access_list(txs, Some(overrides));
    dbg!(access_list);
}
