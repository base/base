//! `payer_sendTransaction` on a running node: the transaction is checked against the node's
//! state before it is forwarded to the payer.

use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, Bytes, U256, hex};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{Call, Eip8130Signed, TxEip8130};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payer_rpc::{
    PayerRejection, PayerRpcConfig, PayerRpcExtension, PayerTransactionError,
};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, build_test_genesis_everest};
use httpmock::prelude::*;
use serde_json::{Value, json};

/// The account named as payer.
const PAYER: Account = Account::Bob;

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

/// A transaction `sender` signs with nonce 0, sponsored by [`PAYER`].
fn sponsored(chain_id: u64, sender: &PrivateKeySigner) -> Eip8130Signed {
    let call = Call { to: Address::repeat_byte(0x11), value: U256::ZERO, data: Bytes::new() };
    let tx = TxEip8130 {
        chain_id,
        max_fee_per_gas: 10_000_000_000,
        gas_limit: 200_000,
        calls: vec![vec![call]],
        payer: Some(PAYER.address()),
        ..Default::default()
    };
    let sender_auth = sender.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes();
    Eip8130Signed::new(tx, Bytes::from(sender_auth.to_vec()), Bytes::new())
}

/// A `payer_sendTransaction` request for `tx`.
fn send_request(tx: &Eip8130Signed) -> Value {
    json!({ "transaction": hex::encode_prefixed(tx.encoded_2718()) })
}

#[tokio::test]
async fn forwards_only_transactions_whose_nonce_is_unused() -> eyre::Result<()> {
    let replaying = PrivateKeySigner::random();
    let fresh = PrivateKeySigner::random();
    let mut genesis = build_test_genesis_everest();
    genesis
        .alloc
        .insert(replaying.address(), GenesisAccount { nonce: Some(1), ..Default::default() });
    let chain_id = genesis.config.chain_id;
    let payer = MockServer::start();
    let forwarded = accepting_payer(&payer);
    let harness = node(genesis, &payer).await?;
    let client = harness.rpc_client()?;

    let error = client
        .request::<_, Value>(
            "payer_sendTransaction",
            (send_request(&sponsored(chain_id, &replaying)),),
        )
        .await
        .expect_err("replayed");
    let payload = error.as_error_resp().expect("JSON-RPC error");
    assert_eq!(payload.code, i64::from(PayerRejection::CODE));
    let replayed: Value =
        serde_json::from_str(payload.data.as_ref().expect("rejection data").get())?;
    assert_eq!(replayed["code"], "INVALID_TRANSACTION");
    assert_eq!(replayed["reason"], PayerTransactionError::NonceUsed.to_string());
    forwarded.assert_calls(0);

    client
        .request::<_, Value>("payer_sendTransaction", (send_request(&sponsored(chain_id, &fresh)),))
        .await?;
    forwarded.assert_calls(1);
    Ok(())
}
