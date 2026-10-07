//! Behavioural regression tests for the standalone EIP-8130
//! `eth_getTransactionCount` and `eth_estimateGas` overrides.
//!
//! These tests pin the dispatch branches of `ChannelNonceReader::read`
//! (protocol-nonce delegation for `nonce_key == 0`, `INVALID_PARAMS` for the
//! `NONCE_KEY_MAX` sentinel, and a real 2D-channel read) and the EIP-8130
//! `eth_estimateGas` path, by exercising the full RPC stack against a test
//! harness. Both the channel read and the estimate are gated on the Everest fork.

use std::{collections::BTreeMap, sync::Arc};

use alloy_eips::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, U256, address, bytes, hex};
use alloy_rpc_client::RpcClient;
use base_common_consensus::{Eip8130Constants, Eip8130Contracts, Predeploys};
use base_common_evm::BaseTime;
use base_common_precompiles::NonceManagerStorage;
use base_execution_chainspec::BaseChainSpec;
use base_execution_eip8130_rpc_node::{Eip8130RpcExtension, Eip8130RpcMode};
use base_node_runner::test_utils::{L1_BLOCK_INFO_DEPOSIT_TX, TestHarness};
use base_protocol::BaseTimeUpdateTx;
use base_test_utils::{Account, build_test_genesis_cobalt, build_test_genesis_everest};
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

/// Everest-activated harness (the common case for EIP-8130 RPC reads).
async fn setup() -> eyre::Result<(TestHarness, RpcClient)> {
    setup_with(build_test_genesis_everest()).await
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
/// there relies on the `valid_before` validity-window bound, not a sequence
/// number.
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
    let mut genesis = build_test_genesis_everest();
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

/// A non-zero `nonce_key` read before the Everest fork must be rejected: EIP-8130
/// RPC features are gated on Everest, mirroring the txpool's pre-activation
/// rejection of EIP-8130 transactions.
#[tokio::test]
async fn nonce_key_pre_everest_is_rejected() -> eyre::Result<()> {
    let (_harness, client) = setup_with(build_test_genesis_cobalt()).await?;
    let alice: Address = Account::Alice.address();

    let result: Result<U256, _> =
        client.request("eth_getTransactionCount", (alice, "latest", U256::from(7u64))).await;

    let err = result.expect_err("pre-Everest nonce_key read must error");
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

/// A supplied secp256k1 authentication blob is priced only if it is well
/// formed: k1 carries exactly 65 signature bytes, so a longer blob is rejected
/// as pool admission rejects it, rather than priced. P-256 and `WebAuthn` blobs
/// are rejected before Zenith, matching txpool admission.
#[tokio::test]
async fn estimate_gas_prices_the_supplied_authentication_blob() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();
    let estimate = |sender_auth: String| {
        let client = client.clone();
        async move {
            client
                .request::<_, U256>(
                    "eth_estimateGas",
                    (json!({ "from": alice, "calls": [], "senderAuth": sender_auth }), "latest"),
                )
                .await
        }
    };

    let gas = estimate(auth_blob(Eip8130Constants::K1_AUTHENTICATOR, 65)).await?;
    assert!(gas > U256::ZERO);
    let err = estimate(auth_blob(Eip8130Constants::K1_AUTHENTICATOR, 200))
        .await
        .expect_err("a malformed k1 blob must be rejected");
    assert!(err.to_string().contains("-32602"), "expected INVALID_PARAMS, got: {err}");

    for authenticator in
        [Eip8130Contracts::P256_AUTHENTICATOR, Eip8130Contracts::WEBAUTHN_AUTHENTICATOR]
    {
        assert!(
            estimate(auth_blob(authenticator, 128)).await.is_err(),
            "a non-k1 sender authenticator ({authenticator}) must be rejected"
        );
    }
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
    let mut genesis = build_test_genesis_everest();
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
/// address: the sender identity drives actor resolution and policy lookup.
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

/// An EIP-8130 `eth_estimateGas` request before the Everest fork must be
/// rejected, matching the `nonce_key` read gate.
#[tokio::test]
async fn estimate_gas_for_eip8130_request_pre_everest_is_rejected() -> eyre::Result<()> {
    let (_harness, client) = setup_with(build_test_genesis_cobalt()).await?;
    let alice: Address = Account::Alice.address();

    let request = json!({ "from": alice, "calls": [] });
    let result: Result<U256, _> = client.request("eth_estimateGas", (request, "latest")).await;

    let err = result.expect_err("pre-Everest EIP-8130 estimate must error");
    let err_str = err.to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// Pending EIP-8130 estimates must observe the scheduled Denim successor's `BaseTime`
/// milliseconds, and an executed pending block's own time once one exists.
#[tokio::test]
async fn estimate_gas_for_eip8130_request_observes_pending_denim_time() -> eyre::Result<()> {
    // Genesis is block 0 at 1s with Denim active, so block n is scheduled at 1s + 200ms * n.
    let (harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();
    let guard = address!("0x00000000000000000000000000000000000000ad");
    // Reverts unless `BaseTime.timestampMs()` equals the first calldata word.
    let guard_code = format!(
        "0x63{}60e01b600052602060006004600073{}5afa5060005160003514603a5760006000fd5b00",
        hex::encode(BaseTime::TIMESTAMP_MS_SELECTOR),
        hex::encode(Predeploys::BASE_TIME),
    );
    let estimate = async |timestamp_ms: u64, base_time_diff: Option<u64>| {
        let mut overrides = json!({ (guard.to_string()): { "code": guard_code } });
        if let Some(millis) = base_time_diff {
            overrides[Predeploys::BASE_TIME.to_string()] = json!({
                "stateDiff": { (B256::ZERO.to_string()): B256::from(U256::from(millis)) }
            });
        }
        let request = json!({
            "from": alice,
            "calls": [[{ "to": guard, "data": format!("0x{timestamp_ms:064x}") }]]
        });
        client.request::<_, U256>("eth_estimateGas", (request, "pending", overrides)).await
    };
    let build_block = async |number: u64, millis_part: u16| {
        let base_time = BaseTimeUpdateTx::new(millis_part)?.into_deposit_tx(number);
        harness
            .prepare_unsafe_block(vec![L1_BLOCK_INFO_DEPOSIT_TX, base_time.encoded_2718().into()])
            .await
    };

    // Same-second successor of genesis, then second rollover after block 4.
    for (latest, timestamp_ms) in [(0, 1_200), (4, 2_000)] {
        while harness.latest_block().number < latest {
            let number = harness.latest_block().number + 1;
            let prepared = build_block(number, u16::try_from(number * 200)?).await?;
            harness
                .engine()
                .update_forkchoice(prepared.parent_hash, prepared.new_block_hash, None)
                .await?;
            harness.wait_for_header(prepared.new_block_hash, prepared.new_block_number).await?;
        }
        assert!(estimate(timestamp_ms, None).await? > U256::ZERO);
        let stale = estimate(timestamp_ms - 200, None).await;
        assert!(stale.unwrap_err().to_string().contains("revert"), "must observe {timestamp_ms}");
    }

    // User state overrides take precedence over the forecast.
    assert!(estimate(2_300, Some(300)).await? > U256::ZERO);

    // An executed pending block is used as-is rather than advanced again.
    build_block(5, 0).await?;
    assert!(estimate(2_000, None).await? > U256::ZERO);
    assert!(estimate(2_200, None).await.is_err());
    Ok(())
}

/// Genesis with a contract at `addr` running `code`.
fn genesis_with_code(addr: Address, code: alloy_primitives::Bytes) -> Genesis {
    let mut genesis = build_test_genesis_everest();
    genesis.alloc.insert(addr, GenesisAccount { code: Some(code), ..Default::default() });
    genesis
}

/// An EIP-8130 `eth_call` runs the EIP-8130 simulation and returns the final
/// call's output, rather than dropping the EIP-8130 fields and simulating an
/// empty transaction.
#[tokio::test]
async fn eth_call_for_eip8130_request_returns_the_call_output() -> eyre::Result<()> {
    let alice: Address = Account::Alice.address();
    // `PUSH1 0x2a PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN`: returns the word 42.
    let returner = address!("0x00000000000000000000000000000000000000fe");
    let (_harness, client) =
        setup_with(genesis_with_code(returner, bytes!("602a60005260206000f3"))).await?;

    let request = json!({ "from": alice, "calls": [[{ "to": returner, "data": "0x" }]] });
    let output: alloy_primitives::Bytes = client.request("eth_call", (request, "latest")).await?;
    assert_eq!(
        output,
        alloy_primitives::Bytes::from(U256::from(42u64).to_be_bytes::<32>().to_vec())
    );
    Ok(())
}

/// An EIP-8130 `eth_call` whose call reverts surfaces the revert, like
/// `eth_estimateGas`, instead of reporting success.
#[tokio::test]
async fn eth_call_for_eip8130_request_with_reverting_call_fails() -> eyre::Result<()> {
    let alice: Address = Account::Alice.address();
    let revert_addr = address!("0x00000000000000000000000000000000000000fd");
    let (_harness, client) =
        setup_with(genesis_with_code(revert_addr, bytes!("60006000fd"))).await?;

    let request = json!({ "from": alice, "calls": [[{ "to": revert_addr, "data": "0x" }]] });
    let result: Result<alloy_primitives::Bytes, _> =
        client.request("eth_call", (request, "latest")).await;
    let err_str = result.expect_err("a reverting call must error").to_string();
    assert!(err_str.contains("revert"), "expected an execution-revert error, got: {err_str}");
    Ok(())
}

/// The standard call paths cannot represent an EIP-8130 transaction, so they
/// reject one rather than silently simulating an empty transaction.
#[tokio::test]
async fn create_access_list_rejects_eip8130_request() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let request = json!({ "from": alice, "calls": [] });
    let result: Result<serde_json::Value, _> =
        client.request("eth_createAccessList", (request, "latest")).await;
    let err_str = result.expect_err("an EIP-8130 access-list request must error").to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// `eth_simulateV1` cannot represent EIP-8130 fields, so it rejects an
/// EIP-8130 call instead of silently simulating a plain transfer.
#[tokio::test]
async fn simulate_v1_rejects_eip8130_request() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let payload = json!({ "blockStateCalls": [{ "calls": [{ "from": alice, "type": "0x79" }] }] });
    let result: Result<serde_json::Value, _> =
        client.request("eth_simulateV1", (payload, "latest")).await;
    let err_str = result.expect_err("an EIP-8130 simulateV1 call must error").to_string();
    assert!(err_str.contains("EIP-8130"), "expected the EIP-8130 rejection, got: {err_str}");
    Ok(())
}

/// A plain `eth_call` still goes through the standard path.
#[tokio::test]
async fn eth_call_for_plain_request_delegates() -> eyre::Result<()> {
    let alice: Address = Account::Alice.address();
    let returner = address!("0x00000000000000000000000000000000000000fe");
    let (_harness, client) =
        setup_with(genesis_with_code(returner, bytes!("602a60005260206000f3"))).await?;

    let output: alloy_primitives::Bytes =
        client.request("eth_call", (json!({ "from": alice, "to": returner }), "latest")).await?;
    assert_eq!(
        output,
        alloy_primitives::Bytes::from(U256::from(42u64).to_be_bytes::<32>().to_vec())
    );
    Ok(())
}

/// Estimation applies the validity-window and nonce-free rules pool admission
/// does: a nonce-free request without `validBefore` is rejected.
#[tokio::test]
async fn estimate_gas_rejects_a_nonce_free_request_without_valid_before() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();

    let request = json!({
        "from": alice,
        "calls": [],
        "nonceKey": format!("{:#x}", Eip8130Constants::NONCE_KEY_MAX),
    });
    let result: Result<U256, _> = client.request("eth_estimateGas", (request, "latest")).await;
    let err_str = result.expect_err("a malformed nonce-free request must error").to_string();
    assert!(err_str.contains("-32602"), "expected INVALID_PARAMS (-32602), got: {err_str}");
    Ok(())
}

/// The validity window is checked at the simulated block's time: a request not
/// yet valid at the head is rejected, but accepted under a `time` block override
/// that opens its window.
#[tokio::test]
async fn eth_call_checks_the_validity_window_at_the_overridden_time() -> eyre::Result<()> {
    let alice: Address = Account::Alice.address();
    let returner = address!("0x00000000000000000000000000000000000000fe");
    let (_harness, client) =
        setup_with(genesis_with_code(returner, bytes!("602a60005260206000f3"))).await?;
    let valid_after_secs = 1_000_u64;
    let request = json!({
        "from": alice,
        "calls": [[{ "to": returner, "data": "0x" }]],
        "validAfter": format!("{valid_after_secs:#x}"),
    });

    let at_head: Result<alloy_primitives::Bytes, _> =
        client.request("eth_call", (&request, "latest")).await;
    let err_str = at_head.expect_err("not yet valid at the head").to_string();
    assert!(err_str.contains("not yet valid"), "expected a validity-window error, got: {err_str}");

    let block_overrides = json!({ "time": format!("{valid_after_secs:#x}") });
    let output: alloy_primitives::Bytes =
        client.request("eth_call", (&request, "latest", json!({}), block_overrides)).await?;
    assert_eq!(
        output,
        alloy_primitives::Bytes::from(U256::from(42u64).to_be_bytes::<32>().to_vec())
    );
    Ok(())
}

/// `type: 0x79` alone marks an EIP-8130 request, so a top-level call is
/// estimated through the EIP-8130 path rather than as a plain transfer.
#[tokio::test]
async fn estimate_gas_treats_type_0x79_as_eip8130() -> eyre::Result<()> {
    let (_harness, client) = setup().await?;
    let alice: Address = Account::Alice.address();
    let bob: Address = Account::Bob.address();

    let request = json!({ "type": "0x79", "from": alice, "to": bob, "value": "0x1" });
    let gas: U256 = client.request("eth_estimateGas", (request, "latest")).await?;
    assert_ne!(gas, U256::from(21_000u64), "priced under the EIP-8130 schedule, not as a transfer");
    Ok(())
}
