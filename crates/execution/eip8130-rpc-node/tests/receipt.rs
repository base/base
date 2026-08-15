//! End-to-end inclusion test for an EIP-8130 (type `0x79`) transaction: sign a
//! minimal EOA-path transaction, mine it into a block, and assert that
//! `eth_getTransactionReceipt` returns a successful receipt.

use std::sync::Arc;

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, Bytes, U256, address, bytes};
use alloy_provider::Provider;
use alloy_signer::SignerSync;
use base_common_consensus::{Call, Eip8130Constants, Eip8130Signed, TxEip8130};
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_test_utils::{Account, DEVNET_CHAIN_ID, build_test_genesis_cobalt};

/// EIP-8130 transaction type byte.
const EIP8130_TX_TYPE: u8 = 0x79;

/// Mines a minimal EOA-path EIP-8130 transaction and asserts its receipt is a
/// successful type `0x79` receipt.
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

    assert_eq!(raw[0], EIP8130_TX_TYPE, "encoded transaction must carry the 0x79 type byte");

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

/// Mines *two* EIP-8130 transactions into a single block — a fully-successful
/// one and one whose second phase reverts — and asserts each receipt carries
/// its own `phaseStatuses`.
///
/// This locks the per-transaction attribution of the thread-local
/// executor→receipt-builder handoff ([`Eip8130PhaseStatuses`]), which relies on
/// reth driving each transaction as `execute` -> `build_receipt` sequentially on
/// one thread. A regression that leaked one transaction's statuses into the next
/// (or swapped them) would surface here — distinct array lengths and contents
/// per transaction — even though the single-8130-tx-per-block tests above would
/// all still pass.
#[tokio::test]
async fn two_eip8130_transactions_in_one_block_attribute_phase_statuses() -> eyre::Result<()> {
    // `PUSH1 0x00, PUSH1 0x00, REVERT` — a contract that always reverts with
    // empty data, seeded into genesis so a phase can be made to revert.
    let revert_addr = address!("0x00000000000000000000000000000000000000fd");
    let mut genesis = build_test_genesis_cobalt();
    genesis.alloc.insert(
        revert_addr,
        GenesisAccount { code: Some(bytes!("60006000fd")), ..Default::default() },
    );
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<Eip8130RpcExtension>(Eip8130RpcMode::Register)
        .build()
        .await?;
    let provider = harness.provider();

    // Transaction 1 (Alice, self-pay): one phase with a single successful call to
    // an EOA. Expected `phaseStatuses == [0x01]`.
    let alice = Account::Alice;
    let tx1 = TxEip8130 {
        chain_id: DEVNET_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        expiry: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: vec![vec![Call { to: Account::Charlie.address(), data: Bytes::new() }]],
        metadata: Bytes::new(),
        payer: None,
    };
    let sender_auth_1: Bytes =
        alice.signer().sign_hash_sync(&tx1.sender_signature_hash())?.as_bytes().to_vec().into();
    let signed_1 = Eip8130Signed::new(tx1, sender_auth_1, Bytes::new());
    let tx1_hash = *signed_1.hash();
    let raw_1: Bytes = signed_1.encoded_2718().into();

    // Transaction 2 (Bob, self-pay): phase 0 succeeds (EOA call), phase 1 reverts
    // (call into the reverting contract). Expected `phaseStatuses == [0x01, 0x00]`
    // and an overall reverted receipt status, while still being included.
    let bob = Account::Bob;
    let tx2 = TxEip8130 {
        chain_id: DEVNET_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence: 0,
        expiry: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: vec![
            vec![Call { to: Account::Charlie.address(), data: Bytes::new() }],
            vec![Call { to: revert_addr, data: Bytes::new() }],
        ],
        metadata: Bytes::new(),
        payer: None,
    };
    let sender_auth_2: Bytes =
        bob.signer().sign_hash_sync(&tx2.sender_signature_hash())?.as_bytes().to_vec().into();
    let signed_2 = Eip8130Signed::new(tx2, sender_auth_2, Bytes::new());
    let tx2_hash = *signed_2.hash();
    let raw_2: Bytes = signed_2.encoded_2718().into();

    // Both EIP-8130 transactions ride in the same block, behind the mandatory
    // L1 block-info deposit.
    harness.build_block_from_transactions(vec![L1_BLOCK_INFO_DEPOSIT_TX, raw_1, raw_2]).await?;

    let receipt_1 = provider
        .get_transaction_receipt(tx1_hash)
        .await?
        .expect("first EIP-8130 transaction must have a receipt");
    let receipt_2 = provider
        .get_transaction_receipt(tx2_hash)
        .await?
        .expect("second EIP-8130 transaction must have a receipt");

    assert_eq!(
        receipt_1.block_number(),
        receipt_2.block_number(),
        "both transactions must be mined into the same block"
    );
    assert!(receipt_1.status(), "the all-success transaction's receipt must report success");
    assert!(
        !receipt_2.status(),
        "the transaction with a reverting phase must report a reverted receipt status"
    );

    let client = harness.rpc_client()?;
    let json_1: serde_json::Value =
        client.request("eth_getTransactionReceipt", (tx1_hash,)).await?;
    let json_2: serde_json::Value =
        client.request("eth_getTransactionReceipt", (tx2_hash,)).await?;

    let payer_1: Address = serde_json::from_value(json_1["payer"].clone())?;
    let payer_2: Address = serde_json::from_value(json_2["payer"].clone())?;
    assert_eq!(payer_1, alice.address(), "first receipt payer must be Alice");
    assert_eq!(payer_2, bob.address(), "second receipt payer must be Bob");

    assert_eq!(
        json_1["phaseStatuses"],
        serde_json::json!(["0x1"]),
        "the all-success transaction must report a single committed phase"
    );
    assert_eq!(
        json_2["phaseStatuses"],
        serde_json::json!(["0x1", "0x0"]),
        "the partially-reverting transaction must report its own committed-then-reverted phases, \
         proving the per-tx phase-status handoff is not cross-contaminated"
    );

    Ok(())
}
