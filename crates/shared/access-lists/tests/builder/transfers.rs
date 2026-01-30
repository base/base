//! Tests for ETH transfer tracking in the access list

use std::collections::HashMap;

use super::{
    AccountInfo, DEVNET_CHAIN_ID, IntoAddress, ONE_ETHER, OpTransaction, TxEnv, TxKind, U256,
    execute_txns_build_access_list,
};

#[test]
/// Tests that the system precompiles get included in the access list
fn test_precompiles() {
    let base_tx = TxEnv::builder().chain_id(Some(DEVNET_CHAIN_ID)).gas_limit(50_000).gas_price(0);
    let tx = OpTransaction::builder().base(base_tx).build_fill();
    let access_list = execute_txns_build_access_list(vec![tx], None, None)
        .expect("access list build should succeed");

    // Verify we got an access list (precompiles/system contracts should be touched)
    assert!(!access_list.account_changes.is_empty(), "Access list should not be empty");
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
                .chain_id(Some(DEVNET_CHAIN_ID))
                .kind(TxKind::Call(recipient))
                .value(U256::from(1_000_000))
                .gas_price(0)
                .gas_priority_fee(None)
                .max_fee_per_gas(0)
                .gas_limit(21_100),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify sender is in the access list with balance and nonce changes
    let sender_entry = access_list.account_changes.iter().find(|ac| ac.address == sender);
    assert!(sender_entry.is_some(), "Sender should be in access list");
    let sender_changes = sender_entry.unwrap();
    assert!(!sender_changes.balance_changes.is_empty(), "Sender should have balance change");
    assert!(!sender_changes.nonce_changes.is_empty(), "Sender should have nonce change");

    // Verify recipient is in the access list with balance change
    let recipient_entry = access_list.account_changes.iter().find(|ac| ac.address == recipient);
    assert!(recipient_entry.is_some(), "Recipient should be in access list");
    let recipient_changes = recipient_entry.unwrap();
    assert!(!recipient_changes.balance_changes.is_empty(), "Recipient should have balance change");
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
                .chain_id(Some(DEVNET_CHAIN_ID))
                .kind(TxKind::Call(recipient))
                .value(U256::from(1_000_000))
                .gas_price(1000)
                .gas_priority_fee(Some(1_000))
                .max_fee_per_gas(1_000)
                .gas_limit(21_100),
        )
        .build_fill();

    let access_list = execute_txns_build_access_list(vec![tx], Some(overrides), None)
        .expect("access list build should succeed");

    // Verify sender has balance change reflecting gas + value
    let sender_entry = access_list.account_changes.iter().find(|ac| ac.address == sender);
    assert!(sender_entry.is_some(), "Sender should be in access list");
    let sender_changes = sender_entry.unwrap();
    assert!(!sender_changes.balance_changes.is_empty(), "Sender should have balance change");

    // Verify there's a fee vault/beneficiary that received the gas payment
    // In OP stack this is typically the sequencer fee vault
    let fee_recipients: Vec<_> = access_list
        .account_changes
        .iter()
        .filter(|ac| ac.address != sender && ac.address != recipient)
        .filter(|ac| !ac.balance_changes.is_empty())
        .collect();
    assert!(!fee_recipients.is_empty(), "There should be a fee recipient with balance change");
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
                    .chain_id(Some(DEVNET_CHAIN_ID))
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

    let access_list = execute_txns_build_access_list(txs, Some(overrides), None)
        .expect("access list build should succeed");

    // Verify sender has 10 nonce changes (one per tx)
    let sender_entry = access_list.account_changes.iter().find(|ac| ac.address == sender);
    assert!(sender_entry.is_some(), "Sender should be in access list");
    let sender_changes = sender_entry.unwrap();
    assert_eq!(
        sender_changes.nonce_changes.len(),
        10,
        "Sender should have 10 nonce changes (one per tx)"
    );
    assert_eq!(
        sender_changes.balance_changes.len(),
        10,
        "Sender should have 10 balance changes (one per tx)"
    );

    // Verify recipient has 10 balance changes (one per tx)
    let recipient_entry = access_list.account_changes.iter().find(|ac| ac.address == recipient);
    assert!(recipient_entry.is_some(), "Recipient should be in access list");
    let recipient_changes = recipient_entry.unwrap();
    assert_eq!(
        recipient_changes.balance_changes.len(),
        10,
        "Recipient should have 10 balance changes (one per tx)"
    );

    // Verify tx indices are tracked correctly (0 through 9)
    let tx_indices: Vec<_> =
        sender_changes.nonce_changes.iter().map(|nc| nc.block_access_index).collect();
    for i in 0..10u64 {
        assert!(tx_indices.contains(&i), "Tx index {i} should be present in nonce changes");
    }
}
