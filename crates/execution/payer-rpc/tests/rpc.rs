//! `payer_sendTransaction` on a running node: the transaction is checked against the node's
//! state before it is forwarded to the payer.

use std::sync::Arc;

use alloy_eips::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, Bytes, U256, address, bytes, hex};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{Panic, PanicKind, SolConstructor, SolError};
use base_common_consensus::{BaseTimeUpdateTx, Call, Eip8130Constants, Eip8130Signed, TxEip8130};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payer_rpc::{
    PayerRejection, PayerRpcConfig, PayerRpcExtension, PayerTransactionError,
};
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_test_utils::{Account, MockERC20, build_test_genesis_everest};
use httpmock::prelude::*;
use serde_json::{Value, json};

/// `PUSH1 0x00, PUSH1 0x00, REVERT`: always reverts with empty data.
const REVERTER: Address = address!("0x00000000000000000000000000000000000000fd");

/// The account named as payer; prefunded so it can co-sign and pay for gas.
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
        })
        .build()
        .await
}

/// A phase of one call to `to` with `data`.
fn phase(to: Address, data: Bytes) -> Vec<Call> {
    vec![Call { to, value: U256::ZERO, data }]
}

/// A transaction `sender` signs with nonce 0, sponsored by [`PAYER`], running `phases` in order.
fn sponsored(chain_id: u64, sender: &PrivateKeySigner, phases: Vec<Vec<Call>>) -> Eip8130Signed {
    let tx = TxEip8130 {
        chain_id,
        max_fee_per_gas: 10_000_000_000,
        gas_limit: 200_000,
        calls: phases,
        payer: Some(PAYER.address()),
        ..Default::default()
    };
    let sender_auth = sender.sign_hash_sync(&tx.sender_signature_hash()).unwrap().as_bytes();
    Eip8130Signed::new(tx, Bytes::from(sender_auth.to_vec()), Bytes::new())
}

/// What the payer does with a forwarded transaction: co-signs it, ready for inclusion.
fn payer_signed(tx: Eip8130Signed, sender: Address) -> eyre::Result<Bytes> {
    let signature = PAYER.signer().sign_hash_sync(&tx.tx().payer_signature_hash(sender))?;
    let payer_auth =
        [Eip8130Constants::K1_AUTHENTICATOR.as_slice(), &signature.as_bytes()].concat();
    let sender_auth = tx.sender_auth().clone();
    Ok(Eip8130Signed::new(tx.into_tx(), sender_auth, payer_auth.into()).encoded_2718().into())
}

/// A `payer_sendTransaction` request for `tx`.
fn send_request(tx: &Eip8130Signed) -> Value {
    json!({ "transaction": hex::encode_prefixed(tx.encoded_2718()) })
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

/// Mines a block of `transactions` behind the deposits every block starts with.
async fn mine(
    harness: &TestHarness,
    transactions: impl IntoIterator<Item = Bytes>,
) -> eyre::Result<()> {
    let block_number = harness.latest_block().number + 1;
    let base_time = BaseTimeUpdateTx::new(200)?.into_deposit_tx(block_number).encoded_2718().into();
    harness
        .build_block_from_transactions(
            [L1_BLOCK_INFO_DEPOSIT_TX, base_time].into_iter().chain(transactions).collect(),
        )
        .await
}

/// Deploys a reverting-on-failure ERC-20 and mints `amount` of it to `holder`.
async fn deploy_token(
    harness: &TestHarness,
    holder: Address,
    amount: U256,
) -> eyre::Result<Address> {
    let deployer = Account::Deployer;
    let constructor = MockERC20::constructorCall {
        _name: "Test USD".into(),
        _symbol: "TUSD".into(),
        _decimals: 6,
    };
    let (deploy, token, _) = deployer.create_deployment_tx(
        [MockERC20::BYTECODE.to_vec(), constructor.abi_encode()].concat().into(),
        0,
    )?;
    let mint = MockERC20::new(token, harness.provider())
        .mint(holder, amount)
        .into_transaction_request()
        .nonce(1);
    let (mint, _) = deployer.sign_txn_request(mint)?;
    mine(harness, [deploy, mint]).await?;
    Ok(token)
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

    let first_phase_reverts = sponsored(chain_id, &sender, vec![phase(REVERTER, Bytes::new())]);
    let reverted = rejection(&harness, send_request(&first_phase_reverts)).await?;
    assert_eq!(reverted["code"], "EXECUTION_REVERTED");
    assert_eq!(reverted["revert"]["phase"], 0);
    forwarded.assert_calls(0);

    let later_phase_reverts = sponsored(
        chain_id,
        &sender,
        vec![phase(Address::repeat_byte(0x11), Bytes::new()), phase(REVERTER, Bytes::new())],
    );
    harness
        .rpc_client()?
        .request::<_, Value>("payer_sendTransaction", (send_request(&later_phase_reverts),))
        .await?;
    forwarded.assert_calls(1);
    Ok(())
}

#[tokio::test]
async fn forwards_only_token_payments_the_sender_can_cover() -> eyre::Result<()> {
    let genesis = build_test_genesis_everest();
    let chain_id = genesis.config.chain_id;
    let payer = MockServer::start();
    let forwarded = accepting_payer(&payer);
    let harness = node(genesis, &payer).await?;
    let sender = PrivateKeySigner::random();
    let token = deploy_token(&harness, sender.address(), U256::from(100)).await?;
    let token = MockERC20::new(token, harness.provider());
    let pay = |amount: u64| {
        let transfer = token.transfer(PAYER.address(), U256::from(amount));
        sponsored(chain_id, &sender, vec![phase(*token.address(), transfer.calldata().clone())])
    };

    let overdrawn = rejection(&harness, send_request(&pay(101))).await?;
    assert_eq!(overdrawn["code"], "EXECUTION_REVERTED");
    assert_eq!(overdrawn["revert"]["phase"], 0);
    assert_eq!(
        overdrawn["revert"]["data"],
        hex::encode_prefixed(Panic::from(PanicKind::UnderOverflow).abi_encode())
    );
    forwarded.assert_calls(0);

    let covered = pay(100);
    harness
        .rpc_client()?
        .request::<_, Value>("payer_sendTransaction", (send_request(&covered),))
        .await?;
    forwarded.assert_calls(1);

    mine(&harness, [payer_signed(covered, sender.address())?]).await?;
    assert_eq!(token.balanceOf(PAYER.address()).call().await?, U256::from(100));
    assert_eq!(token.balanceOf(sender.address()).call().await?, U256::ZERO);
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

    let replay =
        sponsored(chain_id, &sender, vec![phase(Address::repeat_byte(0x11), Bytes::new())]);
    let replayed = rejection(&harness, send_request(&replay)).await?;

    assert_eq!(replayed["code"], "INVALID_TRANSACTION");
    assert_eq!(replayed["reason"], PayerTransactionError::NonceUsed.to_string());
    forwarded.assert_calls(0);
    Ok(())
}
