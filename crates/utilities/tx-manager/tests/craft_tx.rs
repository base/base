//! Integration tests for [`SimpleTxManager`] transaction construction with Anvil.

use alloy_consensus::{TxEip1559, TxEnvelope};
use alloy_eips::{Decodable2718, eip4844::Blob};
use alloy_network::EthereumWallet;
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{
    GasPriceCaps, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig, TxManagerError,
};

/// Helper: spawns an Anvil instance and returns a [`SimpleTxManager`].
async fn setup() -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let chain_id = anvil.chain_id();
    let config = TxManagerConfig::default();
    let manager = SimpleTxManager::new(provider, wallet, config, chain_id)
        .await
        .expect("should create manager");
    (manager, anvil)
}

/// Decodes raw RLP-encoded transaction bytes into the inner [`TxEip1559`].
///
/// Panics if the bytes are not a valid EIP-2718 envelope or the
/// transaction type is not EIP-1559.
fn decode_eip1559(raw: &Bytes) -> TxEip1559 {
    let envelope =
        TxEnvelope::decode_2718(&mut raw.as_ref()).expect("should decode as valid TxEnvelope");
    match envelope {
        TxEnvelope::Eip1559(signed) => signed.strip_signature(),
        other => panic!("expected EIP-1559, got {other:?}"),
    }
}

#[tokio::test]
async fn craft_tx_produces_valid_signed_eip1559_transaction() {
    let (manager, anvil) = setup().await;

    let to = Address::with_last_byte(0x42);
    let value = U256::from(1_000_000_000u64);
    let candidate = TxCandidate {
        to: Some(to),
        value,
        gas_limit: 0, // auto-estimate
        ..Default::default()
    };

    let raw_tx = manager.craft_tx(&candidate).await.expect("should craft tx");
    let tx = decode_eip1559(&raw_tx);

    assert_eq!(tx.to, TxKind::Call(to));
    assert_eq!(tx.value, value);
    assert_eq!(tx.chain_id, anvil.chain_id());
    assert!(tx.max_fee_per_gas > 0, "max_fee_per_gas should be non-zero");
    assert!(tx.max_priority_fee_per_gas > 0, "max_priority_fee_per_gas should be non-zero");
}

#[tokio::test]
async fn craft_tx_with_explicit_gas_limit() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 21_000, // standard ETH transfer gas
        ..Default::default()
    };

    let raw_tx = manager.craft_tx(&candidate).await.expect("should craft tx with explicit gas");
    let tx = decode_eip1559(&raw_tx);

    // Verify the explicit gas limit was used, not an estimated value.
    assert_eq!(tx.gas_limit, 21_000);
}

#[tokio::test]
async fn craft_tx_rejects_blob_transactions() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        blobs: vec![Blob::default()],
        ..Default::default()
    };

    let err = manager.craft_tx(&candidate).await.expect_err("should reject blob tx");
    match &err {
        TxManagerError::Unsupported(msg) => {
            assert!(
                msg.contains("blob transactions are not yet supported"),
                "expected blob rejection message, got: {msg}",
            );
        }
        other => panic!("expected TxManagerError::Unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn craft_tx_contract_creation() {
    let (manager, _anvil) = setup().await;

    // Minimal valid contract bytecode (STOP opcode).
    let candidate = TxCandidate {
        to: None,
        tx_data: Bytes::from_static(&[0x00]),
        gas_limit: 0,
        ..Default::default()
    };

    let raw_tx = manager.craft_tx(&candidate).await.expect("should craft contract creation tx");
    let tx = decode_eip1559(&raw_tx);

    assert_eq!(tx.to, TxKind::Create);
}

#[tokio::test]
async fn suggest_gas_price_caps_returns_valid_estimates() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate::default();
    let caps: GasPriceCaps =
        manager.suggest_gas_price_caps(&candidate).await.expect("should return gas price caps");

    // On an Anvil instance, tip and fee cap should be non-zero.
    assert!(caps.gas_tip_cap > 0, "tip_cap should be non-zero");
    // gas_fee_cap = tip + 2 * base_fee, which is always > tip alone.
    assert!(caps.gas_fee_cap > caps.gas_tip_cap, "fee_cap should exceed tip_cap");
    // Blob fee cap should be None for non-blob transactions.
    assert!(caps.blob_fee_cap.is_none(), "blob_fee_cap should be None");
}

#[tokio::test]
async fn prepare_produces_valid_signed_transaction() {
    let (manager, _anvil) = setup().await;

    let to = Address::with_last_byte(0x42);
    let candidate = TxCandidate {
        to: Some(to),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let raw_tx = manager.prepare(&candidate).await.expect("should prepare tx");
    let tx = decode_eip1559(&raw_tx);

    // Confirm the candidate's fields survive the retry wrapper.
    assert_eq!(tx.to, TxKind::Call(to));
}

#[tokio::test]
async fn prepare_returns_channel_closed_when_manager_is_closed() {
    let (manager, _anvil) = setup().await;

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
    let (manager, anvil) = setup().await;

    let expected_address = anvil.addresses()[0];
    assert_eq!(manager.sender_address(), expected_address);
}

#[tokio::test]
async fn is_closed_reflects_manager_state() {
    let (manager, _anvil) = setup().await;

    assert!(!manager.is_closed());
    manager.close();
    assert!(manager.is_closed());
}

#[tokio::test]
async fn sequential_craft_tx_increments_nonce() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        gas_limit: 0,
        ..Default::default()
    };

    // Craft two transactions — nonces should be sequential.
    let raw_tx1 = manager.craft_tx(&candidate).await.expect("first tx");
    let raw_tx2 = manager.craft_tx(&candidate).await.expect("second tx");

    assert_eq!(decode_eip1559(&raw_tx1).nonce, 0);
    assert_eq!(decode_eip1559(&raw_tx2).nonce, 1);
}

#[tokio::test]
async fn new_rejects_chain_id_mismatch() {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let config = TxManagerConfig::default();

    // Anvil uses chain_id 31337 by default; supply a wrong one.
    let wrong_chain_id = 999;
    let err = SimpleTxManager::new(provider, wallet, config, wrong_chain_id)
        .await
        .expect_err("should reject mismatched chain_id");

    match &err {
        TxManagerError::InvalidConfig(msg) => {
            assert!(
                msg.contains("chain_id mismatch"),
                "expected chain_id mismatch error, got: {msg}",
            );
        }
        other => panic!("expected TxManagerError::InvalidConfig, got {other:?}"),
    }
}

#[test]
fn simple_tx_manager_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SimpleTxManager>();
}
