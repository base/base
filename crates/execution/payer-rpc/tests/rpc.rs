//! `payer_sendTransaction` on a running node: the first call phase runs against the node's
//! state before the transaction is forwarded to the payer.

use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, Bytes, U256, address, bytes, hex};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{BaseTxEnvelope, Call, Eip8130Signed, TxEip8130};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payer_rpc::{PayerRejection, PayerRpcConfig, PayerRpcExtension};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::build_test_genesis_everest;
use httpmock::prelude::*;
use serde_json::{Value, json};

/// `PUSH1 0x00, PUSH1 0x00, REVERT`: always reverts with empty data.
const REVERTER: Address = address!("0x00000000000000000000000000000000000000fd");

/// A `payer_sendTransaction` request for an EOA-signed, sponsored transaction with `phases`.
fn send_request(chain_id: u64, phases: &[Address]) -> Value {
    let signer = PrivateKeySigner::random();
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
    let sender_auth = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes();
    let signed = Eip8130Signed::new(tx, Bytes::from(sender_auth.to_vec()), Bytes::new());
    json!({ "transaction": hex::encode_prefixed(BaseTxEnvelope::Eip8130(signed).encoded_2718()) })
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
    let forwarded = payer.mock(|when, then| {
        when.method(POST).body_includes("payer_sendTransaction");
        then.status(200).header("content-type", "application/json").json_body(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": { "transactionHash": format!("0x{}", "11".repeat(32)) }
        }));
    });
    let harness = TestHarness::builder()
        .with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)))
        .with_ext::<PayerRpcExtension>(PayerRpcConfig {
            upstream_url: payer.base_url(),
            upstream_headers: Vec::new(),
            cache_get_terms: false,
        })
        .build()
        .await?;
    let client = harness.rpc_client()?;

    let reverted = client
        .request::<_, Value>("payer_sendTransaction", (send_request(chain_id, &[REVERTER]),))
        .await
        .expect_err("phase 0 reverts");
    let rejection = reverted.as_error_resp().expect("JSON-RPC error");
    assert_eq!(rejection.code, i64::from(PayerRejection::CODE));
    let data: Value = serde_json::from_str(rejection.data.as_ref().expect("rejection data").get())?;
    assert_eq!(data["code"], "EXECUTION_REVERTED");
    assert_eq!(data["revert"]["phase"], 0);
    forwarded.assert_calls(0);

    let later_phase_reverts = send_request(chain_id, &[Address::repeat_byte(0x11), REVERTER]);
    client.request::<_, Value>("payer_sendTransaction", (later_phase_reverts,)).await?;
    forwarded.assert_calls(1);
    Ok(())
}
