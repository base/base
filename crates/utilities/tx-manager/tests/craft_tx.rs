//! Integration tests for [`SimpleTxManager`] transaction construction with Anvil.

use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{SimpleTxManager, TxCandidate, TxManager, TxManagerConfig, TxManagerError};

/// Helper: spawns an Anvil instance and returns a [`SimpleTxManager`].
fn setup() -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let chain_id = anvil.chain_id();
    let config = TxManagerConfig::default();
    let manager =
        SimpleTxManager::new(provider, wallet, config, chain_id).expect("should create manager");
    (manager, anvil)
}

#[tokio::test]
async fn craft_tx_produces_valid_signed_eip1559_transaction() {
    let (manager, _anvil) = setup();

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000_000_000u64),
        gas_limit: 0, // auto-estimate
        ..Default::default()
    };

    let raw_tx = manager.craft_tx(&candidate).await.expect("should craft tx");

    // Decode the raw bytes back to a TxEnvelope.
    let envelope =
        TxEnvelope::decode_2718(&mut raw_tx.as_ref()).expect("should decode as valid TxEnvelope");

    // Verify it's an EIP-1559 transaction.
    assert!(matches!(envelope, TxEnvelope::Eip1559(_)));
}

#[tokio::test]
async fn craft_tx_with_explicit_gas_limit() {
    let (manager, _anvil) = setup();

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 21_000, // standard ETH transfer gas
        ..Default::default()
    };

    let raw_tx = manager.craft_tx(&candidate).await.expect("should craft tx with explicit gas");

    let envelope =
        TxEnvelope::decode_2718(&mut raw_tx.as_ref()).expect("should decode as valid TxEnvelope");

    assert!(matches!(envelope, TxEnvelope::Eip1559(_)));
}

#[tokio::test]
async fn prepare_produces_valid_signed_transaction() {
    let (manager, _anvil) = setup();

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let raw_tx = manager.prepare(&candidate).await.expect("should prepare tx");

    let envelope =
        TxEnvelope::decode_2718(&mut raw_tx.as_ref()).expect("should decode as valid TxEnvelope");

    assert!(matches!(envelope, TxEnvelope::Eip1559(_)));
}

#[tokio::test]
async fn prepare_returns_channel_closed_when_manager_is_closed() {
    let (manager, _anvil) = setup();

    manager.close();

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::ZERO,
        ..Default::default()
    };

    let err = manager.prepare(&candidate).await.expect_err("should fail");
    assert_eq!(err, TxManagerError::ChannelClosed);
}

#[tokio::test]
async fn sender_address_matches_wallet() {
    let (manager, anvil) = setup();

    let expected_address = anvil.addresses()[0];
    assert_eq!(manager.sender_address(), expected_address);
}

#[tokio::test]
async fn is_closed_reflects_manager_state() {
    let (manager, _anvil) = setup();

    assert!(!manager.is_closed());
    manager.close();
    assert!(manager.is_closed());
}

#[tokio::test]
async fn sequential_craft_tx_increments_nonce() {
    let (manager, _anvil) = setup();

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        gas_limit: 0,
        ..Default::default()
    };

    // Craft two transactions — nonces should be sequential.
    let raw_tx1 = manager.craft_tx(&candidate).await.expect("first tx");
    let raw_tx2 = manager.craft_tx(&candidate).await.expect("second tx");

    let env1 = TxEnvelope::decode_2718(&mut raw_tx1.as_ref()).expect("decode first");
    let env2 = TxEnvelope::decode_2718(&mut raw_tx2.as_ref()).expect("decode second");

    // Extract nonces from the EIP-1559 transactions.
    let nonce1 = match &env1 {
        TxEnvelope::Eip1559(signed) => signed.tx().nonce,
        other => panic!("expected EIP-1559, got {other:?}"),
    };
    let nonce2 = match &env2 {
        TxEnvelope::Eip1559(signed) => signed.tx().nonce,
        other => panic!("expected EIP-1559, got {other:?}"),
    };

    assert_eq!(nonce1, 0);
    assert_eq!(nonce2, 1);
}

#[test]
fn simple_tx_manager_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SimpleTxManager>();
}
