//! `payer_sendTransaction` on a running node: the transaction is checked against the node's
//! state before it is forwarded to the payer.

use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, Bytes, U256, address, bytes, hex};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseTxEnvelope, Call, Eip8130Signed, TxEip8130};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payer_rpc::{
    PayerRejection, PayerRpcConfig, PayerRpcExtension, PayerTransactionError,
};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::build_test_genesis_everest;
use httpmock::prelude::*;
use serde_json::{Value, json};

/// `PUSH1 0x00, PUSH1 0x00, REVERT`: always reverts with empty data.
const REVERTER: Address = address!("0x00000000000000000000000000000000000000fd");

/// A payer that accepts every `payer_sendTransaction`.
fn accepting_payer(payer: &MockServer) -> httpmock::Mock<'_> {
    payer.mock(|when, then| {
        when.method(POST).body_includes("payer_sendTransaction");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": { "transactionHash": format!("0x{}", "11".repeat(32)) }
        }));
    })
}

/// Starts a node on `genesis` that forwards payer methods to `payer`.
async fn node(genesis: Genesis, payer: &MockServer) -> eyre::Result<TestHarness> {
    TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .with_ext::<PayerRpcExtension>(PayerRpcConfig {
            upstream_url: payer.base_url(),
            upstream_headers: Vec::new(),
            cache_get_terms: false,
        })
        .build()
        .await
}

/// A `payer_sendTransaction` request for a sponsored transaction `sender` signs with nonce 0,
/// with one single-call phase per entry of `phases`.
fn send_request(chain_id: u64, sender: &PrivateKeySigner, phases: &[Address]) -> Value {
    let tx = TxEip8130 {
        chain_id,
        max_fee_per_gas: 10_000_000_000,
        gas_limit: 200_000,
        calls: phases
            .iter()
            .map(|&to| vec![Call { to, value: U256::ZERO, data: Bytes::new() }])
            .collect(),
        payer: Some(Address::repeat_byte(0xaa)),
        ..Default::default()
    };
    let sender_auth = sender.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes();
    let signed = Eip8130Signed::new(tx, Bytes::from(sender_auth.to_vec()), Bytes::new());
    json!({ "transaction": hex::encode_prefixed(BaseTxEnvelope::Eip8130(signed).encoded_2718()) })
}

/// Sends `request` and returns the `data` of the node's `PAYER_REJECTED` error.
async fn rejection(harness: &TestHarness, request: Value) -> eyre::Result<Value> {
    let error = harness
        .rpc_client()?
        .request::<_, Value>("payer_sendTransaction", (request,))
        .await
        .expect_err("rejected");
    let payload = error.as_error_resp().expect("JSON-RPC error");
    assert_eq!(payload.code, i64::from(PayerRejection::CODE));
    Ok(serde_json::from_str(payload.data.as_ref().expect("rejection data").get())?)
}

#[tokio::test]
async fn forwards_only_transactions_whose_first_phase_completes() -> eyre::Result<()> {
    let mut genesis = build_test_genesis_everest();
    genesis.alloc.insert(
        REVERTER,
        GenesisAccount { code: Some(bytes!("60006000fd")), ..Default::default() },
    );
    let chain_id = genesis.config.chain_id;
    let payer = MockServer::start();
    let forwarded = accepting_payer(&payer);
    let harness = node(genesis, &payer).await?;
    let sender = PrivateKeySigner::random();

    let reverted = rejection(&harness, send_request(chain_id, &sender, &[REVERTER])).await?;
    assert_eq!(reverted["code"], "EXECUTION_REVERTED");
    assert_eq!(reverted["revert"]["phase"], 0);
    forwarded.assert_calls(0);

    let later_phase_reverts =
        send_request(chain_id, &sender, &[Address::repeat_byte(0x11), REVERTER]);
    harness
        .rpc_client()?
        .request::<_, Value>("payer_sendTransaction", (later_phase_reverts,))
        .await?;
    forwarded.assert_calls(1);
    Ok(())
}

#[tokio::test]
async fn rejects_a_transaction_whose_nonce_was_used() -> eyre::Result<()> {
    let sender = PrivateKeySigner::random();
    let mut genesis = build_test_genesis_everest();
    genesis.alloc.insert(sender.address(), GenesisAccount { nonce: Some(1), ..Default::default() });
    let chain_id = genesis.config.chain_id;
    let payer = MockServer::start();
    let forwarded = accepting_payer(&payer);
    let harness = node(genesis, &payer).await?;

    let replayed =
        rejection(&harness, send_request(chain_id, &sender, &[Address::repeat_byte(0x11)])).await?;

    assert_eq!(replayed["code"], "INVALID_TRANSACTION");
    assert_eq!(replayed["reason"], PayerTransactionError::NonceUsed.to_string());
    forwarded.assert_calls(0);
    Ok(())
}
