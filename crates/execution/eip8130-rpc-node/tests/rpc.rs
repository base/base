//! Behavioural regression tests for the standalone EIP-8130
//! `eth_getTransactionCount` and `eth_estimateGas` overrides.
//!
//! These tests pin the dispatch branches of `ChannelNonceReader::read`
//! (protocol-nonce delegation for `nonce_key == 0`, `INVALID_PARAMS` for the
//! `NONCE_KEY_MAX` sentinel, and a real 2D-channel read) and the EIP-8130
//! `eth_estimateGas` path, by exercising the full RPC stack against a test
//! harness. Both the channel read and the estimate are gated on the Cobalt fork.

use std::{collections::BTreeMap, sync::Arc};

use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, U256, address, bytes};
use alloy_rpc_client::RpcClient;
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};
use base_common_precompiles::NonceManagerStorage;
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_node_runner::test_utils::TestHarness;
use base_test_utils::{Account, build_test_genesis_azul, build_test_genesis_cobalt};
use serde_json::json;

/// Launches a harness with the standalone EIP-8130 override registered over the
/// supplied genesis.
async fn setup_with(genesis: Genesis) -> eyre::Result<(TestHarness, RpcClient)> {
    let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
    let harness = TestHarness::builder()
        .with_chain_spec(chain_spec)
        .with_ext::<Eip8130RpcExtension>(Eip8130RpcMode::Register)
        .build()
        .await?;
    let client = harness.rpc_client()?;
    Ok((harness, client))
}

/// Cobalt-activated harness (the common case for EIP-8130 RPC reads).
async fn setup() -> eyre::Result<(TestHarness, RpcClient)> {
    setup_with(build_test_genesis_cobalt()).await
}

/// A hex (`0x`) authentication blob for an `eth_estimateGas` request: a 20-byte
/// authenticator selector followed by `data_len` filler bytes.
fn auth_blob(authenticator: Address, data_len: usize) -> String {
    let mut v = authenticator.as_slice().to_vec();
    v.resize(v.len() + data_len, 0xff);
    alloy_primitives::hex::encode_prefixed(v)
}

/// `nonce_key == 0` must delegate to the standard protocol-nonce path
/// (`EthState::transaction_count`) rather than reading the precompile.
#[tokio::test]
async fn nonce_key_zero_returns_protocol_nonce() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let legacy: U256 = client.request("eth_getTransactionCount", (alice, "latest")).await?;
    let with_key: U256 =
        client.request("eth_getTransactionCount", (alice, "latest", U256::ZERO)).await?;

    assert_eq!(with_key, legacy, "nonce_key=0 must return the same value as the legacy 2-arg call");
    Ok(())
}

/// `nonce_key == NONCE_KEY_MAX` must return `INVALID_PARAMS` because the
/// expiring-nonce sentinel has no per-channel counter; replay protection
/// there relies on `expiry`, not a sequence number.
#[tokio::test]
async fn nonce_key_max_returns_invalid_params() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let result: Result<U256, _> = client
        .request("eth_getTransactionCount", (alice, "latest", Eip8130Constants::NONCE_KEY_MAX))
        .await;

    let err = result.expect_err("NONCE_KEY_MAX must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// A non-zero `nonce_key` must read the real 2D channel nonce from the Nonce
/// Manager precompile's storage. The channel value is seeded directly into
/// genesis at the derived slot so the read returns it without first executing a
/// nonce-incrementing transaction.
#[tokio::test]
async fn nonce_key_reads_seeded_channel_value() -> eyre::Result<()> {
    let alice: Address = Account::Alice.address();
    let nonce_key = U256::from(7u64);
    let channel_value: u64 = 42;

    // Seed `nonces[alice][7] = 42` into the Nonce Manager precompile's storage.
    let slot = NonceManagerStorage::nonce_slot(alice, nonce_key).expect("non-protocol nonce key");
    let mut genesis = build_test_genesis_cobalt();
    genesis.alloc.insert(
        NonceManagerStorage::ADDRESS,
        GenesisAccount {
            // Non-empty so the seeded storage survives EIP-161 state clearing.
            nonce: Some(1),
            storage: Some(BTreeMap::from([(
                B256::from(slot),
                B256::from(U256::from(channel_value)),
            )])),
            ..Default::default()
        },
    );

    let (_harness, client) = setup_with(genesis).await?;

    let read: U256 =
        client.request("eth_getTransactionCount", (alice, "latest", nonce_key)).await?;
    assert_eq!(read, U256::from(channel_value), "must decode the seeded channel nonce");
    Ok(())
}

/// A non-zero `nonce_key` read before the Cobalt fork must be rejected: EIP-8130
/// RPC features are gated on Cobalt, mirroring the txpool's pre-activation
/// rejection of EIP-8130 transactions.
#[tokio::test]
async fn nonce_key_pre_cobalt_is_rejected() -> eyre::Result<()> {
    let (_harness, client) = setup_with(build_test_genesis_azul()).await?;
    let alice: Address = Account::Alice.address();

    let result: Result<U256, _> =
        client.request("eth_getTransactionCount", (alice, "latest", U256::from(7u64))).await;

    let err = result.expect_err("pre-Cobalt nonce_key read must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// An `eth_estimateGas` request carrying EIP-8130 fields must estimate via the
/// read-only simulation path and return a positive gas amount. A minimal
/// EOA-path request (`from` + empty `calls`) prices intrinsic + authentication
/// gas without a signature.
#[tokio::test]
async fn estimate_gas_for_eip8130_request_returns_positive_gas() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let request = json!({ "from": alice, "calls": [] });
    let gas: U256 = client.request("eth_estimateGas", (request, "latest")).await?;

    assert!(gas > U256::ZERO, "EIP-8130 gas estimate must be positive, got {gas}");
    Ok(())
}

/// The account may be named by the EIP-8130 `sender` field instead of `from`;
/// a configured-account request estimates to a positive gas amount.
#[tokio::test]
async fn estimate_gas_accepts_explicit_sender() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let request = json!({ "sender": alice, "calls": [] });
    let gas: U256 = client.request("eth_estimateGas", (request, "latest")).await?;

    assert!(gas > U256::ZERO, "EIP-8130 gas estimate must be positive, got {gas}");
    Ok(())
}

/// A request naming the account by both `from` and `sender` with disagreeing
/// values is rejected rather than guessing which to trust.
#[tokio::test]
async fn estimate_gas_rejects_mismatched_from_and_sender() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();
    let bob: Address = Account::Bob.address();

    let request = json!({ "from": alice, "sender": bob, "calls": [] });
    let result: Result<U256, _> = client.request("eth_estimateGas", (request, "latest")).await;

    let err = result.expect_err("a `from`/`sender` mismatch must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// A supplied non-secp256k1 authentication blob must be priced into the
/// estimate: a P-256 sender costs strictly more than the default-EOA secp256k1
/// path (its authenticator execution gas is higher and its authentication
/// payload is longer), and a longer `WebAuthn` blob costs more still.
#[tokio::test]
async fn estimate_gas_prices_the_supplied_authentication_blob() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let k1: U256 = client
        .request("eth_estimateGas", (json!({ "from": alice, "calls": [] }), "latest"))
        .await?;
    let p256: U256 = client
        .request(
            "eth_estimateGas",
            (
                json!({
                    "from": alice,
                    "calls": [],
                    "senderAuth": auth_blob(Eip8130Contracts::P256_AUTHENTICATOR, 128),
                }),
                "latest",
            ),
        )
        .await?;
    let webauthn: U256 = client
        .request(
            "eth_estimateGas",
            (
                json!({
                    "from": alice,
                    "calls": [],
                    "senderAuth": auth_blob(Eip8130Contracts::WEBAUTHN_AUTHENTICATOR, 1024),
                }),
                "latest",
            ),
        )
        .await?;

    assert!(p256 > k1, "P-256 auth ({p256}) must cost more than secp256k1 ({k1})");
    assert!(
        webauthn > p256,
        "a larger WebAuthn payload ({webauthn}) must cost more than P-256 ({p256})"
    );
    Ok(())
}

/// A declared sponsoring `payer` must add payer authentication gas on top of the
/// self-pay estimate for the same calls.
#[tokio::test]
async fn estimate_gas_includes_sponsored_payer_authentication() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();
    let bob: Address = Account::Bob.address();

    let self_pay: U256 = client
        .request("eth_estimateGas", (json!({ "from": alice, "calls": [] }), "latest"))
        .await?;
    let sponsored: U256 = client
        .request("eth_estimateGas", (json!({ "from": alice, "calls": [], "payer": bob }), "latest"))
        .await?;

    assert!(
        sponsored > self_pay,
        "sponsored estimate ({sponsored}) must exceed self-pay ({self_pay})"
    );
    Ok(())
}

/// An EIP-8130 `eth_estimateGas` whose phased call reverts must fail like the
/// standard estimator: the simulation surfaces an execution error rather than a
/// gas number for a call that would not succeed. The transaction would still be
/// included on-chain, but estimation mirrors `eth_estimateGas`/`eth_call`.
#[tokio::test]
async fn estimate_gas_for_eip8130_request_with_reverting_call_fails() -> eyre::Result<()> {
    let alice: Address = Account::Alice.address();
    // `PUSH1 0x00, PUSH1 0x00, REVERT` — always reverts with empty data.
    let revert_addr = address!("0x00000000000000000000000000000000000000fd");
    let mut genesis = build_test_genesis_cobalt();
    genesis.alloc.insert(
        revert_addr,
        GenesisAccount { code: Some(bytes!("60006000fd")), ..Default::default() },
    );
    let (_harness, client) = setup_with(genesis).await?;

    let request = json!({ "from": alice, "calls": [[{ "to": revert_addr, "data": "0x" }]] });
    let result: Result<U256, _> = client.request("eth_estimateGas", (request, "latest")).await;

    let err = result.expect_err("a reverting phase must surface an execution error");
    let err_str = err.to_string();
    assert!(err_str.contains("revert"), "expected an execution-revert error, got: {err_str}");
    Ok(())
}

/// An EIP-8130 `eth_estimateGas` request that names no account (neither `from`
/// nor `sender`) must be rejected rather than silently simulated from the zero
/// address: the sender identity drives actor resolution, policy lookup, and
/// auto-delegation.
#[tokio::test]
async fn estimate_gas_for_eip8130_request_without_account_is_rejected() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;

    let request = json!({ "calls": [] });
    let result: Result<U256, _> = client.request("eth_estimateGas", (request, "latest")).await;

    let err = result.expect_err("EIP-8130 estimate without an account must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// A plain (non-8130) `eth_estimateGas` request must still work through the
/// override, which delegates to the standard reth estimator. A bare value
/// transfer estimates to the 21000-gas floor.
#[tokio::test]
async fn estimate_gas_for_plain_request_delegates() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();
    let bob: Address = Account::Bob.address();

    let request = json!({ "from": alice, "to": bob, "value": "0x1" });
    let gas: U256 = client.request("eth_estimateGas", (request, "latest")).await?;

    assert_eq!(gas, U256::from(21_000u64), "plain transfer estimates to the base gas floor");
    Ok(())
}

/// An EIP-8130 `eth_estimateGas` request before the Cobalt fork must be
/// rejected, matching the `nonce_key` read gate.
#[tokio::test]
async fn estimate_gas_for_eip8130_request_pre_cobalt_is_rejected() -> eyre::Result<()> {
    let (_harness, client) = setup_with(build_test_genesis_azul()).await?;
    let alice: Address = Account::Alice.address();

    let request = json!({ "from": alice, "calls": [] });
    let result: Result<U256, _> = client.request("eth_estimateGas", (request, "latest")).await;

    let err = result.expect_err("pre-Cobalt EIP-8130 estimate must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}
