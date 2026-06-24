//! End-to-end inclusion test for an EIP-8130 (type `0x7b`) transaction: sign a
//! minimal EOA-path transaction, mine it into a block, and assert that
//! `eth_getTransactionReceipt` returns a successful receipt.

use std::sync::Arc;

use alloy_eips::eip2718::Encodable2718;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_consensus::{Call, Eip8130Constants, Eip8130Signed, TxEip8130};
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis_cobalt};

/// EIP-8130 transaction type byte.
const EIP8130_TX_TYPE: u8 = 0x7b;

/// Mines a minimal EOA-path EIP-8130 transaction and asserts its receipt is a
/// successful type `0x7b` receipt.
#[tokio::test]
async fn eip8130_transaction_is_mined_and_has_a_receipt() -> eyre::Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_cobalt()));
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<Eip8130RpcExtension>(Eip8130RpcMode::Register)
        .build()
        .await?;
    let provider = harness.provider();

    // Minimal EOA self-pay transaction: no account changes, no calls, protocol
    // nonce channel at sequence 0 (Alice's starting nonce). The sender is
    // recovered from `sender_auth`, so `sender` is left `None`.
    let alice = Account::Alice;
    let tx = TxEip8130 {
        chain_id: DEVNET_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        expiry: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: None,
    };

    let signature = alice.signer().sign_hash_sync(&tx.sender_signature_hash())?;
    let sender_auth: Bytes = signature.as_bytes().to_vec().into();
    let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());
    let tx_hash = *signed.hash();
    let raw: Bytes = signed.encoded_2718().into();

    assert_eq!(raw[0], EIP8130_TX_TYPE, "encoded transaction must carry the 0x7b type byte");

    // The L1 block-info deposit must lead every block.
    harness.build_block_from_transactions(vec![L1_BLOCK_INFO_DEPOSIT_TX, raw]).await?;

    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .expect("mined EIP-8130 transaction must have a receipt");

    assert!(receipt.status(), "EIP-8130 transaction receipt must report success");
    assert!(receipt.gas_used() > 0, "receipt must report non-zero gas used");
    assert_eq!(receipt.transaction_hash(), tx_hash, "receipt must reference the submitted tx");
    assert!(receipt.block_number().is_some(), "receipt must be mined into a block");

    // The EIP-8130 RPC receipt carries the gas payer; for a self-pay transaction
    // that is the resolved sender. With empty `calls`, `phaseStatuses` is omitted.
    let client = harness.rpc_client()?;
    let json: serde_json::Value = client.request("eth_getTransactionReceipt", (tx_hash,)).await?;
    let payer: Address = serde_json::from_value(json["payer"].clone())?;
    assert_eq!(payer, alice.address(), "self-pay receipt payer must be the sender");
    assert!(
        json.get("phaseStatuses").is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())),
        "empty-calls transaction must not report phase statuses"
    );
    assert!(
        json.get("metadata").is_none_or(serde_json::Value::is_null),
        "empty metadata must be omitted, not serialized as \"0x\""
    );

    Ok(())
}

/// Mines an EIP-8130 transaction with a single successful call phase and asserts
/// its receipt reports `phaseStatuses == [0x01]`.
#[tokio::test]
async fn eip8130_receipt_reports_phase_statuses() -> eyre::Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_cobalt()));
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<Eip8130RpcExtension>(Eip8130RpcMode::Register)
        .build()
        .await?;
    let provider = harness.provider();

    // Self-pay transaction with one phase containing a single value-less call to
    // an EOA (Bob), which succeeds and yields a single `0x01` phase status.
    let alice = Account::Alice;
    let tx = TxEip8130 {
        chain_id: DEVNET_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        expiry: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: vec![vec![Call { to: Account::Bob.address(), data: Bytes::new() }]],
        metadata: Bytes::from_static(&[0xab, 0xcd]),
        payer: None,
    };

    let signature = alice.signer().sign_hash_sync(&tx.sender_signature_hash())?;
    let sender_auth: Bytes = signature.as_bytes().to_vec().into();
    let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());
    let tx_hash = *signed.hash();
    let raw: Bytes = signed.encoded_2718().into();

    harness.build_block_from_transactions(vec![L1_BLOCK_INFO_DEPOSIT_TX, raw]).await?;

    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .expect("mined EIP-8130 transaction must have a receipt");
    assert!(receipt.status(), "EIP-8130 transaction receipt must report success");

    let client = harness.rpc_client()?;
    let json: serde_json::Value = client.request("eth_getTransactionReceipt", (tx_hash,)).await?;
    let payer: Address = serde_json::from_value(json["payer"].clone())?;
    assert_eq!(payer, alice.address(), "self-pay receipt payer must be the sender");
    assert_eq!(
        json["phaseStatuses"],
        serde_json::json!(["0x1"]),
        "single successful phase must report one 0x01 status"
    );
    assert_eq!(
        json["metadata"], "0xabcd",
        "receipt must surface the transaction's EIP-8130 metadata"
    );

    Ok(())
}

/// Mines a *sponsored* EIP-8130 transaction (declared payer != sender) and
/// asserts the receipt reports the declared payer rather than the sender,
/// locking the `tx.payer.unwrap_or(sender)` precedence at RPC.
#[tokio::test]
async fn eip8130_sponsored_receipt_reports_declared_payer() -> eyre::Result<()> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(build_test_genesis_cobalt()));
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<Eip8130RpcExtension>(Eip8130RpcMode::Register)
        .build()
        .await?;
    let provider = harness.provider();

    // Alice sends; Bob sponsors the gas. Bob authenticates over the payer digest
    // (which binds to the resolved sender) with his K1 (secp256k1) authenticator.
    let alice = Account::Alice;
    let bob = Account::Bob;
    let tx = TxEip8130 {
        chain_id: DEVNET_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        expiry: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: Some(bob.address()),
    };

    let sender_auth: Bytes =
        alice.signer().sign_hash_sync(&tx.sender_signature_hash())?.as_bytes().to_vec().into();
    // Explicit (non-recovered) payer auth is `authenticator(20) || data`; for the
    // K1 authenticator the data is Bob's 65-byte signature over the payer digest.
    let payer_sig = bob.signer().sign_hash_sync(&tx.payer_signature_hash(alice.address()))?;
    let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    payer_auth.extend_from_slice(&payer_sig.as_bytes());
    let signed = Eip8130Signed::new(tx, sender_auth, payer_auth.into());
    let tx_hash = *signed.hash();
    let raw: Bytes = signed.encoded_2718().into();

    harness.build_block_from_transactions(vec![L1_BLOCK_INFO_DEPOSIT_TX, raw]).await?;

    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .expect("mined sponsored EIP-8130 transaction must have a receipt");
    assert!(receipt.status(), "sponsored EIP-8130 transaction receipt must report success");

    let client = harness.rpc_client()?;
    let json: serde_json::Value = client.request("eth_getTransactionReceipt", (tx_hash,)).await?;
    let payer: Address = serde_json::from_value(json["payer"].clone())?;
    assert_eq!(payer, bob.address(), "sponsored receipt payer must be the declared payer");
    assert_ne!(payer, alice.address(), "sponsored receipt payer must not be the sender");

    Ok(())
}
